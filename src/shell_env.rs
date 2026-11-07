//! `PATH` for apps started outside a terminal.
//!
//! A macOS `.app` opened from Finder (or an AppImage started from a Linux
//! desktop) gets a minimal `PATH` such as `/usr/bin:/bin:/usr/sbin:/sbin`, so
//! `opencode2` installed with npm under nvm/Volta/Homebrew isn't found, and
//! opencode's own tools (git, rg, compilers) would be missing too. Like other
//! editors, ocdesk asks the user's login shell for its `PATH` once at startup
//! and puts it first. Started from a terminal, the inherited `PATH` is
//! already right and nothing changes. `OCDESK_NO_SHELL_ENV=1` opts out.

use std::io::{IsTerminal as _, Read as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START: &str = "__OCDESK_PATH_START__";
const END: &str = "__OCDESK_PATH_END__";
/// rc files can be slow (nvm); never block startup longer than this.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Call first thing in `main`, before any other thread exists (it sets an
/// environment variable). Returns what it did, to log once logging is up.
pub fn import_login_path() -> Option<String> {
    if !cfg!(unix)
        || std::env::var_os("OCDESK_NO_SHELL_ENV").is_some()
        || std::io::stdin().is_terminal()
    {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "/bin/zsh".into()
        } else {
            "/bin/sh".into()
        }
    });
    let current = std::env::var("PATH").unwrap_or_default();
    let Some(shell_path) = login_path(&shell) else {
        return Some(format!(
            "could not read PATH from {shell}; keeping {current}"
        ));
    };
    let merged = merge_paths(&shell_path, &current);
    // SAFETY: called at the top of `main`, before any other thread is
    // spawned, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("PATH", &merged) };
    Some(format!("PATH from {shell}: {merged}"))
}

fn login_path(shell: &str) -> Option<String> {
    let fish = shell.rsplit('/').next().is_some_and(|n| n == "fish");
    // fish's $PATH is a list; quoting it would join with spaces.
    let script = if fish {
        format!("printf '%s' '{START}'(string join : $PATH)'{END}'")
    } else {
        format!("printf '%s' \"{START}${{PATH}}{END}\"")
    };
    // Interactive too: nvm and friends usually live in .zshrc/.bashrc.
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    extract(&out)
}

/// The PATH between the markers; rc files may print banners around it.
fn extract(out: &str) -> Option<String> {
    let start = out.rfind(START)? + START.len();
    let len = out[start..].find(END)?;
    let path = out[start..start + len].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// `first`'s entries, then `rest`'s that aren't already there.
fn merge_paths(first: &str, rest: &str) -> String {
    let mut seen = Vec::new();
    for p in first.split(':').chain(rest.split(':')) {
        if !p.is_empty() && !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_between_markers() {
        let out = format!("Welcome!\n{START}/a/bin:/usr/bin{END}\nbye");
        assert_eq!(extract(&out).as_deref(), Some("/a/bin:/usr/bin"));
        assert_eq!(extract("no markers"), None);
        assert_eq!(extract(&format!("{START}{END}")), None);
    }

    #[test]
    fn merge_keeps_order_and_dedups() {
        assert_eq!(
            merge_paths("/nvm/bin:/usr/bin", "/usr/bin:/bin::/sbin"),
            "/nvm/bin:/usr/bin:/bin:/sbin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_path_from_sh() {
        // Plain sh accepts -l -i -c; the result must contain something.
        let p = login_path("/bin/sh").expect("sh prints PATH");
        assert!(p.contains('/'));
    }
}
