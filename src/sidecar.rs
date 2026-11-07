//! Spawn, monitor and kill the `opencode serve` child process.
//!
//! We do not use `opencode_codes::ManagedServer` because it inherits stderr
//! (we want the last log lines for crash reporting), hides the child pid (we
//! need it for the panic hook / quit hook) and does not report crashes.
//!
//! Teardown paths, all funnelling into [`kill_sidecar`]:
//! - `App::on_app_quit` (Cmd-Q, last window closed)
//! - the panic hook installed by [`install_panic_hook`]
//! - SIGINT / SIGTERM to ocdesk (see `main.rs`)
//! - [`SidecarGuard`] drop (normal return from `main`)
//! - Linux only: `PR_SET_PDEATHSIG`, so even `kill -9 ocdesk` takes the child down.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use rand::distr::{Alphanumeric, SampleString};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

/// The opencode release ocdesk is developed and tested against.
pub const PINNED_OPENCODE_VERSION: &str = "1.19.5";

/// Basic-auth username used by `opencode serve`.
pub const USERNAME: &str = "opencode";

const STDERR_RING: usize = 200;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Process-group id of the running sidecar, or 0. Read from the panic hook and
/// signal handlers, which cannot reach the GPUI state.
static SIDECAR_PGID: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub binary: PathBuf,
    pub working_dir: PathBuf,
}

/// A running sidecar. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Sidecar {
    pub base_url: String,
    pub password: String,
    pub version: String,
}

/// How the sidecar process ended.
#[derive(Clone, Debug)]
pub struct SidecarExit {
    pub code: Option<i32>,
    pub last_lines: Vec<String>,
}

/// `opencode --version`, trimmed.
pub async fn probe_version(binary: &PathBuf) -> Result<String> {
    let out = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to run `{} --version`", binary.display()))?;
    if !out.status.success() {
        bail!(
            "`{} --version` exited with {}",
            binary.display(),
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Find `opencode` on PATH, then in the usual install locations.
pub fn find_binary() -> Option<PathBuf> {
    let candidates = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(dirs::home_dir().map(|h| h.join(".opencode/bin")))
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    candidates
        .map(|dir| dir.join("opencode"))
        .find(|p| p.is_file())
}

/// Spawn `opencode serve` and wait until it answers `GET /global/health`.
///
/// Returns the sidecar plus a receiver that fires once when the child exits.
pub async fn spawn(cfg: SidecarConfig) -> Result<(Sidecar, oneshot::Receiver<SidecarExit>)> {
    let version = probe_version(&cfg.binary).await?;
    if version != PINNED_OPENCODE_VERSION {
        tracing::warn!(
            found = %version,
            pinned = PINNED_OPENCODE_VERSION,
            "opencode version mismatch; ocdesk is tested against the pinned version"
        );
    }

    let password = Alphanumeric.sample_string(&mut rand::rng(), 32);
    let mut cmd = Command::new(&cfg.binary);
    // `--port 0` does NOT mean "random port" in 1.19.5: it tries 4096 first and
    // only falls back to a random port when 4096 is taken. We pass 0 anyway and
    // treat the `listening on` line on stdout as the source of truth.
    cmd.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        "0",
        "--print-logs",
    ])
    .current_dir(&cfg.working_dir)
    .env("OPENCODE_SERVER_PASSWORD", &password)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{} serve`", cfg.binary.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("sidecar exited immediately"))?;
    SIDECAR_PGID.store(pid as i32, Ordering::SeqCst);
    tracing::info!(pid, version = %version, dir = %cfg.working_dir.display(), "sidecar spawned");

    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_RING)));
    {
        let stderr = child.stderr.take().expect("stderr piped");
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "sidecar", "{line}");
                let mut t = tail.lock().expect("stderr ring poisoned");
                if t.len() == STDERR_RING {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let base_url = tokio::time::timeout(STARTUP_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            tracing::debug!(target: "sidecar", "stdout: {line}");
            if let Some(url) = line.split("listening on ").nth(1) {
                return Ok::<_, anyhow::Error>(url.trim().trim_end_matches('/').to_string());
            }
        }
        bail!("opencode serve exited before reporting a listening address")
    })
    .await
    .map_err(|_| anyhow!("opencode serve did not start within {STARTUP_TIMEOUT:?}"))
    .and_then(|r| r);
    let base_url = match base_url {
        Ok(url) => url,
        Err(e) => {
            kill_sidecar();
            return Err(e.context(tail_context(&stderr_tail)));
        }
    };
    // Keep draining stdout so the child never blocks on a full pipe.
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "sidecar", "stdout: {line}");
        }
    });

    let sidecar = Sidecar {
        base_url,
        password,
        version,
    };
    if let Err(e) = wait_healthy(&sidecar).await {
        kill_sidecar();
        return Err(e.context(tail_context(&stderr_tail)));
    }

    let (exit_tx, exit_rx) = oneshot::channel();
    let tail = stderr_tail;
    tokio::spawn(async move {
        let status = child.wait().await;
        SIDECAR_PGID
            .compare_exchange(pid as i32, 0, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
        let code = status.ok().and_then(|s| s.code());
        let last_lines: Vec<String> = {
            let t = tail.lock().expect("stderr ring poisoned");
            t.iter().rev().take(20).rev().cloned().collect()
        };
        tracing::warn!(?code, "sidecar exited");
        let _ = exit_tx.send(SidecarExit { code, last_lines });
    });

    tracing::info!(url = %sidecar.base_url, "sidecar healthy");
    Ok((sidecar, exit_rx))
}

fn tail_context(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
    let t = tail.lock().expect("stderr ring poisoned");
    let last: Vec<&str> = t.iter().rev().take(10).rev().map(String::as_str).collect();
    format!("sidecar stderr (last lines):\n{}", last.join("\n"))
}

async fn wait_healthy(sidecar: &Sidecar) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/global/health", sidecar.base_url);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let res = client
            .get(&url)
            .basic_auth(USERNAME, Some(&sidecar.password))
            .timeout(Duration::from_millis(500))
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
                bail!("sidecar rejected our credentials (401)")
            }
            _ if Instant::now() > deadline => bail!("sidecar never became healthy"),
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Terminate the sidecar's whole process group. Idempotent and safe to call
/// from a panic hook or signal-handling thread.
pub fn kill_sidecar() {
    let pgid = SIDECAR_PGID.swap(0, Ordering::SeqCst);
    if pgid <= 0 {
        return;
    }
    tracing::info!(pgid, "killing sidecar process group");
    #[cfg(unix)]
    // SAFETY: plain FFI call with integer arguments.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(150));
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Kills the sidecar when dropped. Held by `main` as the last line of defence.
pub struct SidecarGuard;

impl Drop for SidecarGuard {
    fn drop(&mut self) {
        kill_sidecar();
    }
}

pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        kill_sidecar();
        prev(info);
    }));
}
