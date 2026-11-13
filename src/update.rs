//! Replacing the installed copy of ocdesk with a downloaded release, and
//! starting it again. Local, blocking work: call it off the UI thread.
//!
//! What can be replaced depends on how ocdesk was installed:
//! - an AppImage: the one file named by `$APPIMAGE`;
//! - the tarball's `install.sh` layout: `<prefix>/bin/ocdesk` (plus the
//!   desktop entry and icons under `<prefix>/share`);
//! - macOS: the `ocdesk.app` bundle.
//!
//! Anything else (a source build, a system package, a read-only location) is
//! not touched: the user gets a link to the release instead. Every
//! replacement is staged next to its destination and moved into place with
//! a rename, so a failure leaves the old version intact.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::domain::update::{Package, Target};

const DESKTOP_ENTRY: &str = "share/applications/io.github.bmz1.ocdesk.desktop";

/// The files a running ocdesk may replace to update itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    AppImage { file: PathBuf },
    Tarball { exe: PathBuf, prefix: PathBuf },
    MacApp { bundle: PathBuf },
}

impl Installed {
    /// The release file this install is updated from.
    pub(crate) fn target(&self) -> Target {
        Target {
            package: match self {
                Self::AppImage { .. } => Package::AppImage,
                Self::Tarball { .. } => Package::Tarball,
                Self::MacApp { .. } => Package::MacApp,
            },
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

/// How this process was installed, or why it can't update itself.
///
/// Call it at startup: after an update replaced the executable, its path
/// can no longer be read back from the OS.
pub fn detect() -> Result<Installed, String> {
    let exe = std::env::current_exe().map_err(|e| format!("unknown executable path: {e}"))?;
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    classify(
        &exe,
        appimage.as_deref(),
        std::env::consts::OS,
        &|p| p.exists(),
        &is_writable_dir,
    )
}

#[cfg(unix)]
fn is_writable_dir(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the whole call.
    dir.is_dir() && unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0
}

#[cfg(not(unix))]
fn is_writable_dir(_: &Path) -> bool {
    false
}

fn classify(
    exe: &Path,
    appimage: Option<&Path>,
    os: &str,
    exists: &dyn Fn(&Path) -> bool,
    writable_dir: &dyn Fn(&Path) -> bool,
) -> Result<Installed, String> {
    let read_only = |what: &str, dir: &Path| {
        format!(
            "{what} is in {}, which you can't write to",
            dir.to_string_lossy()
        )
    };
    match os {
        "macos" => {
            // …/ocdesk.app/Contents/MacOS/ocdesk
            let bundle = exe
                .parent()
                .filter(|p| p.ends_with("Contents/MacOS"))
                .and_then(|p| p.parent()?.parent())
                .filter(|p| p.extension().is_some_and(|e| e == "app"))
                .ok_or("this copy isn't an ocdesk.app bundle")?;
            // Gatekeeper runs a quarantined app from a random read-only
            // copy until it is moved; a disk image is read-only as well.
            if bundle
                .components()
                .any(|c| c.as_os_str() == "AppTranslocation")
                || bundle.starts_with("/Volumes")
            {
                return Err("move ocdesk to the Applications folder to update it in place".into());
            }
            let dir = bundle.parent().ok_or("unknown application folder")?;
            if !writable_dir(dir) {
                return Err(read_only("ocdesk.app", dir));
            }
            Ok(Installed::MacApp {
                bundle: bundle.to_path_buf(),
            })
        }
        "linux" => {
            if let Some(file) = appimage.filter(|f| f.is_absolute() && exists(f)) {
                let dir = file.parent().ok_or("unknown AppImage folder")?;
                if !writable_dir(dir) {
                    return Err(read_only("the AppImage", dir));
                }
                return Ok(Installed::AppImage {
                    file: file.to_path_buf(),
                });
            }
            // install.sh: <prefix>/bin/ocdesk beside <prefix>/share/….
            let bin = exe.parent().filter(|p| p.ends_with("bin"));
            let prefix = bin
                .and_then(Path::parent)
                .filter(|prefix| exists(&prefix.join(DESKTOP_ENTRY)))
                .ok_or("this copy wasn't installed from a release package")?;
            let bin = prefix.join("bin");
            if !writable_dir(&bin) {
                return Err(read_only("ocdesk", &bin));
            }
            Ok(Installed::Tarball {
                exe: exe.to_path_buf(),
                prefix: prefix.to_path_buf(),
            })
        }
        other => Err(format!("in-place updates aren't supported on {other}")),
    }
}

/// An empty directory for one download. Older downloads are removed.
pub fn fresh_staging_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| anyhow!("no cache directory"))?
        .join("ocdesk")
        .join("updates");
    remove_dir_if_present(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

fn remove_dir_if_present(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

/// Replace the installed files with the verified `download`.
pub fn install(installed: &Installed, download: &Path) -> Result<()> {
    match installed {
        Installed::AppImage { file } => replace_executable(download, file),
        Installed::Tarball { exe, prefix } => install_tarball(download, exe, prefix),
        Installed::MacApp { bundle } => install_mac_app(download, bundle),
    }
}

/// Put a copy of `new` at `dest`, atomically. A running `dest` keeps
/// executing the old file; the next launch gets the new one.
fn replace_executable(new: &Path, dest: &Path) -> Result<()> {
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow!("no file name: {}", dest.display()))?
        .to_string_lossy();
    // Same directory, so the rename can't cross file systems.
    let staged = dest.with_file_name(format!(".{name}.update-{}", std::process::id()));
    let result = (|| {
        std::fs::copy(new, &staged).with_context(|| format!("write next to {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::File::open(&staged)?.sync_all()?;
        std::fs::rename(&staged, dest).with_context(|| format!("replace {}", dest.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {what}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail: String = stderr.trim().chars().take(300).collect();
        bail!("{what} failed ({}): {detail}", output.status);
    }
    Ok(())
}

/// The release tarball holds one directory: `ocdesk`, `install.sh`, `share/`.
fn install_tarball(download: &Path, exe: &Path, prefix: &Path) -> Result<()> {
    let unpacked = download.with_extension("unpacked");
    remove_dir_if_present(&unpacked)?;
    std::fs::create_dir_all(&unpacked)?;
    let result = (|| {
        run(
            Command::new("tar")
                .arg("-xzf")
                .arg(download)
                .arg("-C")
                .arg(&unpacked)
                .arg("--no-same-owner"),
            "tar",
        )?;
        let mut entries = std::fs::read_dir(&unpacked)?.collect::<std::io::Result<Vec<_>>>()?;
        let root = match (entries.pop(), entries.is_empty()) {
            (Some(only), true) => only.path(),
            _ => bail!("unexpected archive layout"),
        };
        let binary = root.join("ocdesk");
        if !std::fs::symlink_metadata(&binary).is_ok_and(|m| m.is_file()) {
            bail!("the archive has no ocdesk executable");
        }
        replace_executable(&binary, exe)?;
        // The launcher entry and icons rarely change; the binary is what
        // matters, so a failure here is only logged.
        for sub in ["share/applications", "share/icons"] {
            if let Err(e) = copy_tree(&root.join(sub), &prefix.join(sub)) {
                tracing::warn!(op = "update_install", dir = sub, error = %format!("{e:#}"),
                    "could not refresh desktop files");
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&unpacked);
    result
}

/// Copy regular files and directories of `from` over `to` (no links).
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Unzip the new bundle beside the installed one, check its signature is
/// intact, then swap the two with renames.
fn install_mac_app(download: &Path, bundle: &Path) -> Result<()> {
    let dir = bundle
        .parent()
        .ok_or_else(|| anyhow!("no folder: {}", bundle.display()))?;
    let stage = dir.join(format!(".ocdesk-update-{}", std::process::id()));
    remove_dir_if_present(&stage)?;
    std::fs::create_dir_all(&stage).with_context(|| format!("write to {}", dir.display()))?;
    let result = (|| {
        // ditto keeps symlinks, permissions and extended attributes, which
        // the code signature covers.
        run(
            Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(download)
                .arg(&stage),
            "ditto",
        )?;
        let new = stage.join("ocdesk.app");
        if !new.join("Contents/MacOS/ocdesk").is_file() {
            bail!("the archive has no ocdesk.app");
        }
        run(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&new),
            "codesign",
        )?;
        let previous = stage.join("previous.app");
        std::fs::rename(bundle, &previous).with_context(|| format!("move {}", bundle.display()))?;
        if let Err(e) = std::fs::rename(&new, bundle) {
            // Put the running version back.
            let _ = std::fs::rename(&previous, bundle);
            return Err(e).with_context(|| format!("replace {}", bundle.display()));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Waits for process `$1` to exit (30 s at most), then starts `$2`. Values
/// are positional parameters, never spliced into the script.
const RELAUNCH: &str = r#"
i=0
while kill -0 "$1" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -gt 300 ] && exit 1
    sleep 0.1
done
if [ "$3" = open ]; then exec /usr/bin/open "$2"; fi
exec "$2"
"#;

fn relaunch_command(installed: &Installed, pid: u32) -> Command {
    let (program, how) = match installed {
        Installed::AppImage { file } => (file, "exec"),
        Installed::Tarball { exe, .. } => (exe, "exec"),
        // LaunchServices starts the bundle, as a click in Finder would.
        Installed::MacApp { bundle } => (bundle, "open"),
    };
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(RELAUNCH)
        .arg("ocdesk-relaunch")
        .arg(pid.to_string())
        .arg(program)
        .arg(how)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group: quitting (and the sidecar teardown) must not
    // take the relauncher down with us.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
}

/// Start the installed version once this process has exited. The caller
/// quits right after.
pub fn relaunch_after_exit(installed: &Installed) -> Result<()> {
    relaunch_command(installed, std::process::id())
        .spawn()
        .context("could not schedule the restart")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ocdesk-update-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn classify_with(
        exe: &str,
        appimage: Option<&str>,
        os: &str,
        existing: &[&str],
        read_only: &[&str],
    ) -> Result<Installed, String> {
        classify(
            Path::new(exe),
            appimage.map(Path::new),
            os,
            &|p| existing.iter().any(|e| Path::new(e) == p),
            &|p| !read_only.iter().any(|e| Path::new(e) == p),
        )
    }

    #[test]
    fn linux_installs_are_recognized_by_their_layout() {
        // An AppImage runs from a temporary mount; $APPIMAGE is the file.
        assert_eq!(
            classify_with(
                "/tmp/.mount_ocdeskAbC/usr/bin/ocdesk",
                Some("/home/u/Apps/ocdesk.AppImage"),
                "linux",
                &["/home/u/Apps/ocdesk.AppImage"],
                &[],
            ),
            Ok(Installed::AppImage {
                file: "/home/u/Apps/ocdesk.AppImage".into()
            })
        );
        let desktop = "/home/u/.local/share/applications/io.github.bmz1.ocdesk.desktop";
        assert_eq!(
            classify_with("/home/u/.local/bin/ocdesk", None, "linux", &[desktop], &[]),
            Ok(Installed::Tarball {
                exe: "/home/u/.local/bin/ocdesk".into(),
                prefix: "/home/u/.local".into()
            })
        );
        // A stale $APPIMAGE (inherited from another app) is not ours.
        assert!(matches!(
            classify_with(
                "/home/u/.local/bin/ocdesk",
                Some("/gone.AppImage"),
                "linux",
                &[desktop],
                &[]
            ),
            Ok(Installed::Tarball { .. })
        ));
    }

    #[test]
    fn builds_and_read_only_installs_are_left_alone() {
        // cargo build output: no desktop entry beside it, and not in bin/.
        for exe in [
            "/src/ocdesk/target/release/ocdesk",
            "/home/u/bin/ocdesk",
            "/ocdesk",
        ] {
            let why = classify_with(exe, None, "linux", &[], &[]).unwrap_err();
            assert!(why.contains("release package"), "{why}");
        }
        // A system package or a root install: recognized, but not writable.
        let why = classify_with(
            "/usr/bin/ocdesk",
            None,
            "linux",
            &["/usr/share/applications/io.github.bmz1.ocdesk.desktop"],
            &["/usr/bin"],
        )
        .unwrap_err();
        assert!(why.contains("/usr/bin"), "{why}");
        let why = classify_with(
            "/tmp/.mount_x/usr/bin/ocdesk",
            Some("/opt/ocdesk.AppImage"),
            "linux",
            &["/opt/ocdesk.AppImage"],
            &["/opt"],
        )
        .unwrap_err();
        assert!(why.contains("/opt"), "{why}");
        assert!(classify_with("C:/ocdesk.exe", None, "windows", &[], &[]).is_err());
    }

    #[test]
    fn mac_bundles_update_unless_translocated_mounted_or_read_only() {
        let exe = "/Applications/ocdesk.app/Contents/MacOS/ocdesk";
        assert_eq!(
            classify_with(exe, None, "macos", &[], &[]),
            Ok(Installed::MacApp {
                bundle: "/Applications/ocdesk.app".into()
            })
        );
        for exe in [
            "/private/var/folders/x/T/AppTranslocation/1234/d/ocdesk.app/Contents/MacOS/ocdesk",
            "/Volumes/ocdesk/ocdesk.app/Contents/MacOS/ocdesk",
        ] {
            let why = classify_with(exe, None, "macos", &[], &[]).unwrap_err();
            assert!(why.contains("Applications"), "{why}");
        }
        assert!(classify_with(exe, None, "macos", &[], &["/Applications"]).is_err());
        // `cargo run` on a Mac.
        assert!(classify_with("/src/ocdesk/target/debug/ocdesk", None, "macos", &[], &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn appimage_is_replaced_atomically_and_stays_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("appimage");
        let file = dir.join("ocdesk.AppImage");
        std::fs::write(&file, "old").unwrap();
        let download = dir.join("download");
        std::fs::write(&download, "new").unwrap();
        // The running image stays open, as the kernel keeps an executable.
        let running = std::fs::File::open(&file).unwrap();
        install(&Installed::AppImage { file: file.clone() }, &download).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let mut old = String::new();
        std::io::Read::read_to_string(&mut &running, &mut old).unwrap();
        assert_eq!(old, "old", "the old inode is untouched, not truncated");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "no staging file left: {names:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_replacement_keeps_the_installed_file() {
        let dir = scratch("failed");
        let file = dir.join("ocdesk.AppImage");
        std::fs::write(&file, "old").unwrap();
        let missing = dir.join("no-such-download");
        assert!(install(&Installed::AppImage { file: file.clone() }, &missing).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A tarball shaped like `packaging/linux/package.sh` makes it.
    #[cfg(unix)]
    fn release_tarball(dir: &Path, binary: Option<&str>) -> PathBuf {
        let root = dir.join("build/ocdesk-9.9.9-linux-x86_64");
        std::fs::create_dir_all(root.join("share/applications")).unwrap();
        std::fs::create_dir_all(root.join("share/icons/hicolor/64x64/apps")).unwrap();
        if let Some(binary) = binary {
            std::fs::write(root.join("ocdesk"), binary).unwrap();
        }
        std::fs::write(root.join("install.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            root.join("share/applications/io.github.bmz1.ocdesk.desktop"),
            "new entry",
        )
        .unwrap();
        std::fs::write(root.join("share/icons/hicolor/64x64/apps/icon.png"), "png").unwrap();
        let archive = dir.join("ocdesk-9.9.9-linux-x86_64.tar.gz");
        let status = Command::new("tar")
            .arg("-C")
            .arg(dir.join("build"))
            .arg("-czf")
            .arg(&archive)
            .arg("ocdesk-9.9.9-linux-x86_64")
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    #[cfg(unix)]
    #[test]
    fn tarball_replaces_the_binary_and_refreshes_desktop_files() {
        let dir = scratch("tarball");
        let prefix = dir.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/applications")).unwrap();
        let exe = prefix.join("bin/ocdesk");
        std::fs::write(&exe, "old").unwrap();
        let entry = prefix.join(DESKTOP_ENTRY);
        std::fs::write(&entry, "old entry").unwrap();
        let archive = release_tarball(&dir, Some("new"));
        let installed = Installed::Tarball {
            exe: exe.clone(),
            prefix: prefix.clone(),
        };
        install(&installed, &archive).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "new entry");
        assert!(
            prefix
                .join("share/icons/hicolor/64x64/apps/icon.png")
                .exists()
        );
        assert!(
            !archive.with_extension("unpacked").exists(),
            "cleaned up after itself"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_tarball_without_the_binary_or_a_broken_one_changes_nothing() {
        let dir = scratch("badtar");
        let prefix = dir.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let exe = prefix.join("bin/ocdesk");
        std::fs::write(&exe, "old").unwrap();
        let installed = Installed::Tarball {
            exe: exe.clone(),
            prefix: prefix.clone(),
        };
        let empty = release_tarball(&dir, None);
        let error = install(&installed, &empty).unwrap_err();
        assert!(format!("{error:#}").contains("no ocdesk executable"));
        let garbage = dir.join("garbage.tar.gz");
        std::fs::write(&garbage, "not gzip").unwrap();
        let error = install(&installed, &garbage).unwrap_err();
        assert!(format!("{error:#}").contains("tar failed"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert!(!prefix.join("share").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The relauncher must start the new version only after the old process
    /// is gone, and must not start anything if it never exits.
    #[cfg(unix)]
    #[test]
    fn relaunch_waits_for_the_old_process_to_exit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("relaunch");
        let marker = dir.join("started");
        let program = dir.join("ocdesk.AppImage");
        std::fs::write(
            &program,
            format!("#!/bin/sh\necho started > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let installed = Installed::AppImage {
            file: program.clone(),
        };

        let mut old = Command::new("sleep").arg("30").spawn().unwrap();
        let mut relauncher = relaunch_command(&installed, old.id()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(!marker.exists(), "started while the old process was alive");
        assert!(relauncher.try_wait().unwrap().is_none());
        old.kill().unwrap();
        old.wait().unwrap();
        assert!(relauncher.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "started\n");
        let _ = std::fs::remove_dir_all(dir);
    }
}
