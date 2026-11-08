//! Finding and launching the Chromium that serves the agent's browser tools.
//!
//! A system Chrome / Chromium / Edge, headless, with its own profile under
//! ocdesk's cache directory (never the user's real profile), driven over
//! `--remote-debugging-pipe`. `OCDESK_CHROME` overrides the binary.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result, anyhow};

use super::cdp::{Cdp, pipe};

/// Viewport the agent's pages render at (CSS pixels).
pub const VIEWPORT: (u32, u32) = (1280, 800);

pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCDESK_CHROME").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let mac = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    if cfg!(target_os = "macos")
        && let Some(p) = mac.iter().map(PathBuf::from).find(|p| p.is_file())
    {
        return Some(p);
    }
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
        "brave-browser",
    ];
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

pub fn profile_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ocdesk")
        .join("browser-profile")
}

pub struct Chrome {
    pub cdp: Cdp,
    pub child: tokio::process::Child,
}

impl Chrome {
    pub async fn launch(binary: &Path) -> Result<Self> {
        let profile = profile_dir();
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("create {}", profile.display()))?;
        // Chrome reads fd 3 and writes fd 4.
        let (cmd_r, cmd_w) = pipe()?;
        let (ev_r, ev_w) = pipe()?;
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args([
            "--headless=new",
            "--remote-debugging-pipe",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            "--hide-scrollbars",
            &format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1),
            &format!("--user-data-dir={}", profile.display()),
        ]);
        // Chrome refuses to sandbox as root (containers, CI).
        #[cfg(unix)]
        if unsafe { libc::geteuid() } == 0 {
            cmd.arg("--no-sandbox");
        }
        cmd.arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (child_in, child_out) = (cmd_r.as_raw_fd(), ev_w.as_raw_fd());
        // SAFETY: only async-signal-safe calls (dup2, prctl) between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(child_in, 3) < 0 || libc::dup2(child_out, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("launch {}", binary.display()))?;
        // The child has its copies; ours of its ends must close so EOF works.
        drop::<(OwnedFd, OwnedFd)>((cmd_r, ev_w));
        let cdp = Cdp::new(cmd_w, ev_r);
        // The first reply proves the pipe works.
        cdp.call("Browser.getVersion", serde_json::json!({}), None)
            .await
            .map_err(|e| {
                anyhow!(
                    "{} didn't answer on the debugging pipe: {e:#}",
                    binary.display()
                )
            })?;
        Ok(Self { cdp, child })
    }
}
