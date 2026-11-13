//! App updates: which release is newer, which of its files fits this
//! install, and the download → install → restart lifecycle.
//!
//! The release manifest is the `SHA256SUMS.txt` every release carries: one
//! `<sha256>  <file>` line per installer. File names encode the version and
//! the platform (`ocdesk-<version>-linux-<arch>.AppImage`, …), so one small
//! static file answers "is there a newer version", "which file" and "what
//! must its checksum be". Fetching it, downloading and replacing files belong
//! to `api::update`, `crate::update` and the controller; nothing here does I/O.

use std::fmt;

/// A released version: `major.minor.patch`, nothing else. Pre-releases and
/// build metadata do not parse, so they are never offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
        let mut next = || {
            let part = parts.next()?;
            // `u64::from_str` accepts a leading `+`; a version doesn't.
            (!part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        };
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How this copy of ocdesk was installed, as far as replacing it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Package {
    /// A single `.AppImage` file.
    AppImage,
    /// The tarball's `install.sh` layout (`<prefix>/bin/ocdesk`).
    Tarball,
    /// `ocdesk.app` (the universal build).
    MacApp,
}

/// The installer file this install can be updated from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) package: Package,
    /// `x86_64` or `aarch64` (as `uname -m` and `std::env::consts::ARCH`).
    pub(crate) arch: String,
}

impl Target {
    fn file_name(&self, version: Version) -> String {
        match self.package {
            Package::AppImage => format!("ocdesk-{version}-linux-{}.AppImage", self.arch),
            Package::Tarball => format!("ocdesk-{version}-linux-{}.tar.gz", self.arch),
            Package::MacApp => format!("ocdesk-{version}-macos-universal.zip"),
        }
    }
}

/// One downloadable file of a release and the checksum it must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Asset {
    pub(crate) name: String,
    /// Lowercase hex SHA-256.
    pub(crate) sha256: String,
}

/// A release newer than the running version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    pub(crate) version: Version,
    /// `None`: the release has no file this install can be replaced with
    /// (a source build, a system package, an unknown platform). The user is
    /// pointed to the release page instead.
    pub(crate) asset: Option<Asset>,
}

/// Release file names are used in URLs and as local file names: nothing but
/// plain name characters, and no leading dot.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `ocdesk-<version>-<platform…>` → version.
fn version_of(name: &str) -> Option<Version> {
    let rest = name.strip_prefix("ocdesk-")?;
    let end = ["-linux-", "-macos-"]
        .iter()
        .filter_map(|platform| rest.find(platform))
        .min()?;
    Version::parse(&rest[..end])
}

/// The newest version in `manifest` (`sha256sum` output) if it is newer than
/// `current`, with the file for `target` when the release has one.
///
/// Unreadable lines are skipped; `Err` means the manifest names no ocdesk
/// release at all, which is a broken feed rather than "up to date".
pub(crate) fn newer_release(
    manifest: &str,
    current: Version,
    target: Option<&Target>,
) -> Result<Option<Release>, &'static str> {
    let files: Vec<(Version, &str, &str)> = manifest
        .lines()
        .filter_map(|line| {
            let (sha, name) = line.split_once(char::is_whitespace)?;
            // `sha256sum` marks binary mode with `*` before the name.
            let name = name.trim().trim_start_matches('*');
            (is_sha256(sha) && is_plain_name(name)).then_some(())?;
            Some((version_of(name)?, name, sha))
        })
        .collect();
    let Some(latest) = files.iter().map(|(version, ..)| *version).max() else {
        return Err("the release has no readable checksum list");
    };
    if latest <= current {
        return Ok(None);
    }
    let asset = target.and_then(|target| {
        let wanted = target.file_name(latest);
        files
            .iter()
            .find(|(version, name, _)| *version == latest && *name == wanted)
            .map(|(_, name, sha)| Asset {
                name: name.to_string(),
                sha256: sha.to_ascii_lowercase(),
            })
    });
    Ok(Some(Release {
        version: latest,
        asset,
    }))
}

/// Download and installation of one approved update.
///
/// Idle → Downloading → Installing → Ready, or → Failed (retryable). Nothing
/// leaves Idle without [`Install::begin`], which the controller calls only for
/// the user's explicit request. Ready is terminal for this process: the new
/// version is on disk and runs after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Install {
    #[default]
    Idle,
    Downloading {
        version: Version,
        received: u64,
        /// `None` when the server sent no length.
        total: Option<u64>,
    },
    /// Downloaded and verified; replacing the installed files.
    Installing {
        version: Version,
    },
    Ready {
        version: Version,
    },
    Failed {
        version: Version,
        error: String,
    },
}

impl Install {
    /// Start downloading `version`. Refused while another download or
    /// install runs, and once an update is installed.
    pub(crate) fn begin(&mut self, version: Version) -> bool {
        if !matches!(self, Self::Idle | Self::Failed { .. }) {
            return false;
        }
        *self = Self::Downloading {
            version,
            received: 0,
            total: None,
        };
        true
    }

    /// Download progress. Ignored unless downloading; never moves backwards.
    pub(crate) fn progress(&mut self, bytes: u64, of: Option<u64>) -> bool {
        match self {
            Self::Downloading {
                received, total, ..
            } if bytes > *received || of != *total => {
                *received = bytes.max(*received);
                *total = of;
                true
            }
            _ => false,
        }
    }

    /// The download was verified; the installed files are being replaced.
    pub(crate) fn downloaded(&mut self) -> bool {
        match *self {
            Self::Downloading { version, .. } => {
                *self = Self::Installing { version };
                true
            }
            _ => false,
        }
    }

    /// The running download/install ended.
    pub(crate) fn finish(&mut self, result: Result<(), String>) -> bool {
        let (Self::Downloading { version, .. } | Self::Installing { version }) = *self else {
            return false;
        };
        *self = match result {
            Ok(()) => Self::Ready { version },
            Err(error) => Self::Failed { version, error },
        };
        true
    }

    /// Hide a failure. Running work and an installed update stay.
    pub(crate) fn dismiss(&mut self) -> bool {
        let failed = matches!(self, Self::Failed { .. });
        if failed {
            *self = Self::Idle;
        }
        failed
    }

    /// Download progress in percent, when the size is known.
    pub(crate) fn percent(&self) -> Option<u8> {
        match *self {
            Self::Downloading {
                received,
                total: Some(total),
                ..
            } if total > 0 => Some((received.min(total) * 100 / total) as u8),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn target(package: Package, arch: &str) -> Target {
        Target {
            package,
            arch: arch.into(),
        }
    }

    const A: &str = "88184fbe1392149e873a7b80aa35c59c315bfa462bb85f25c8fed31d828286b5";
    const B: &str = "e4a973f1464804d534fc3efbac7a8259fa42969e983692553ce5f2c808a0d22f";
    const C: &str = "76a2fde19b9746c1f059bb23f65b0b7587d64c37fddde3eb269c66ba6d1a0ca7";
    const D: &str = "a150abdc2f211942ba77fa8d045d82ef09675d1105d8c62cd06a11dd0d730209";
    const E: &str = "c2cb2a6469f9c09af69c590dcdfcf3a31da87beb8583f421f4066030492583e6";
    const F: &str = "d8113e4b74c881740b1aa5929daa0e07b421b4aef6716ee77370135a732898b5";

    /// The real v0.2.0 `SHA256SUMS.txt`.
    fn manifest() -> String {
        format!(
            "{A}  ocdesk-0.2.0-linux-aarch64.AppImage\n\
             {B}  ocdesk-0.2.0-linux-aarch64.tar.gz\n\
             {C}  ocdesk-0.2.0-linux-x86_64.AppImage\n\
             {D}  ocdesk-0.2.0-linux-x86_64.tar.gz\n\
             {E}  ocdesk-0.2.0-macos-universal.dmg\n\
             {F}  ocdesk-0.2.0-macos-universal.zip\n"
        )
    }

    #[test]
    fn versions_parse_strictly_and_order_numerically() {
        assert_eq!(v("0.2.0"), v("v0.2.0"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1.2.3-rc.1",
            "1.2.3+build",
            "+1.2.3",
            "1..3",
            " 1.2.3",
            "1.2.99999999999999999999",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn each_install_gets_its_own_file_and_checksum() {
        let cases = [
            (Package::AppImage, "x86_64", C, "linux-x86_64.AppImage"),
            (Package::AppImage, "aarch64", A, "linux-aarch64.AppImage"),
            (Package::Tarball, "x86_64", D, "linux-x86_64.tar.gz"),
            (Package::Tarball, "aarch64", B, "linux-aarch64.tar.gz"),
            // The .dmg is for people, the .zip for the updater.
            (Package::MacApp, "aarch64", F, "macos-universal.zip"),
            (Package::MacApp, "x86_64", F, "macos-universal.zip"),
        ];
        for (package, arch, sha, suffix) in cases {
            let release = newer_release(&manifest(), v("0.1.0"), Some(&target(package, arch)))
                .unwrap()
                .unwrap();
            assert_eq!(release.version, v("0.2.0"));
            assert_eq!(
                release.asset,
                Some(Asset {
                    name: format!("ocdesk-0.2.0-{suffix}"),
                    sha256: sha.into()
                })
            );
        }
    }

    #[test]
    fn the_same_or_an_older_release_is_not_an_update() {
        let t = target(Package::AppImage, "x86_64");
        assert_eq!(newer_release(&manifest(), v("0.2.0"), Some(&t)), Ok(None));
        assert_eq!(newer_release(&manifest(), v("0.3.0"), Some(&t)), Ok(None));
    }

    #[test]
    fn a_newer_release_without_a_fitting_file_is_still_announced() {
        // No target (source build), or an architecture the release lacks.
        for t in [None, Some(target(Package::AppImage, "riscv64"))] {
            let release = newer_release(&manifest(), v("0.1.0"), t.as_ref())
                .unwrap()
                .unwrap();
            assert_eq!(release.version, v("0.2.0"));
            assert_eq!(release.asset, None);
        }
    }

    #[test]
    fn unreadable_lines_are_skipped_and_an_empty_feed_is_an_error() {
        let t = target(Package::Tarball, "x86_64");
        let noisy = format!(
            "# comment\n\
             \n\
             nothex  ocdesk-9.0.0-linux-x86_64.tar.gz\n\
             {A}  ../ocdesk-9.0.0-linux-x86_64.tar.gz\n\
             {A}  ocdesk-9.0.0-rc.1-linux-x86_64.tar.gz\n\
             {A}  other-9.0.0-linux-x86_64.tar.gz\n\
             {A}  .ocdesk-9.0.0-linux-x86_64.tar.gz\n\
             {D} *ocdesk-0.2.0-linux-x86_64.tar.gz\r\n"
        );
        let release = newer_release(&noisy, v("0.1.0"), Some(&t))
            .unwrap()
            .unwrap();
        assert_eq!(release.version, v("0.2.0"));
        assert_eq!(
            release.asset.unwrap().name,
            "ocdesk-0.2.0-linux-x86_64.tar.gz"
        );
        assert!(newer_release("", v("0.1.0"), Some(&t)).is_err());
        assert!(newer_release("<html>not found</html>", v("0.1.0"), Some(&t)).is_err());
    }

    #[test]
    fn a_file_of_an_older_version_never_stands_in_for_the_newest() {
        // A half-uploaded release: the newest version lacks this platform.
        let mixed = format!(
            "{C}  ocdesk-0.2.0-linux-x86_64.AppImage\n\
             {F}  ocdesk-0.3.0-macos-universal.zip\n"
        );
        let release = newer_release(
            &mixed,
            v("0.1.0"),
            Some(&target(Package::AppImage, "x86_64")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(release.version, v("0.3.0"));
        assert_eq!(release.asset, None);
    }

    #[test]
    fn checksums_are_normalized_to_lowercase() {
        let upper = format!("{}  ocdesk-0.2.0-macos-universal.zip\n", F.to_uppercase());
        let release = newer_release(
            &upper,
            v("0.1.0"),
            Some(&target(Package::MacApp, "aarch64")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(release.asset.unwrap().sha256, F);
    }

    #[test]
    fn install_runs_download_install_ready() {
        let mut i = Install::default();
        assert!(!i.progress(1, None), "nothing to report on while idle");
        assert!(i.begin(v("0.3.0")));
        assert!(!i.begin(v("0.3.0")), "one download at a time");
        assert!(i.progress(50, Some(200)));
        assert_eq!(i.percent(), Some(25));
        assert!(!i.progress(50, Some(200)), "no change, no redraw");
        assert!(!i.progress(10, Some(200)), "progress never goes back");
        assert_eq!(i.percent(), Some(25));
        assert!(i.downloaded());
        assert_eq!(
            i,
            Install::Installing {
                version: v("0.3.0")
            }
        );
        assert!(!i.progress(200, Some(200)));
        assert!(!i.begin(v("0.3.0")));
        assert!(i.finish(Ok(())));
        assert_eq!(
            i,
            Install::Ready {
                version: v("0.3.0")
            }
        );
        // Installed: nothing restarts or replaces it in this process.
        assert!(!i.begin(v("0.4.0")));
        assert!(!i.finish(Err("late".into())));
        assert!(!i.dismiss());
    }

    #[test]
    fn a_failure_keeps_its_version_and_can_be_retried_or_dismissed() {
        let mut i = Install::default();
        i.begin(v("0.3.0"));
        assert!(i.finish(Err("checksum mismatch".into())));
        assert_eq!(
            i,
            Install::Failed {
                version: v("0.3.0"),
                error: "checksum mismatch".into()
            }
        );
        assert!(!i.finish(Ok(())), "a duplicate completion changes nothing");
        assert!(i.begin(v("0.3.0")), "retry");
        assert!(i.downloaded());
        assert!(i.finish(Err("read-only".into())), "installing can fail too");
        assert!(i.dismiss());
        assert_eq!(i, Install::Idle);
        assert!(!i.downloaded() && !i.finish(Ok(())));
    }

    #[test]
    fn percent_needs_a_known_nonzero_size_and_is_capped() {
        let mut i = Install::default();
        i.begin(v("0.3.0"));
        assert_eq!(i.percent(), None);
        i.progress(10, Some(0));
        assert_eq!(i.percent(), None);
        i.progress(300, Some(200));
        assert_eq!(i.percent(), Some(100));
    }
}
