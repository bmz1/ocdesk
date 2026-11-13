//! The update feed: ocdesk's own GitHub releases, over HTTPS.
//!
//! Two requests, both plain file downloads (no GitHub API, so no rate limit
//! and no token): the latest release's `SHA256SUMS.txt`, which names the
//! version and every installer's checksum (see `domain::update`), and, only
//! after the user asked for it, one installer. The installer is streamed to
//! disk and must match the manifest's SHA-256 before it is handed on.
//!
//! This is the only code that talks to anything but the local sidecar. It
//! sends no identifiers: a `User-Agent` with the ocdesk version, nothing else.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use super::HttpError;
use crate::domain::update::{Asset, Version};

/// Where releases are published. Asset URLs are
/// `<RELEASES>/download/v<version>/<file>`; `latest/download/<file>`
/// redirects to the newest published, non-prerelease one.
pub const RELEASES: &str = "https://github.com/bmz1/ocdesk/releases";

/// A different feed root, for tests and mirrors. Plain HTTP is accepted for
/// loopback only.
const FEED_ENV: &str = "OCDESK_UPDATE_FEED";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A download that sends nothing for this long is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A dozen lines in practice.
const MAX_MANIFEST: usize = 256 * 1024;
/// Installers are ~50 MB; anything near this is not one of ours.
const MAX_DOWNLOAD: u64 = 1024 * 1024 * 1024;

/// The page describing `version`, for "release notes" and manual downloads.
pub fn release_page(version: Version) -> String {
    format!("{RELEASES}/tag/v{version}")
}

#[derive(Clone)]
pub struct UpdateFeed {
    http: reqwest::Client,
    base: String,
}

impl UpdateFeed {
    /// The configured feed ([`RELEASES`] unless `OCDESK_UPDATE_FEED` is set).
    pub fn from_env() -> Result<Self> {
        let base = std::env::var(FEED_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| RELEASES.to_string());
        Self::new(&base)
    }

    pub fn new(base: &str) -> Result<Self> {
        let base = base.trim().trim_end_matches('/').to_string();
        let url = reqwest::Url::parse(&base).with_context(|| format!("update feed {base:?}"))?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let mut http = reqwest::Client::builder()
            .user_agent(concat!("ocdesk/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT);
        http = match (url.scheme(), loopback) {
            // Redirects (GitHub sends two) must stay on HTTPS as well.
            ("https", _) => http.https_only(true),
            ("http", true) => http.no_proxy(),
            _ => bail!("update feed must be https: {base}"),
        };
        Ok(Self {
            http: http.build()?,
            base,
        })
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        let response = self.http.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(HttpError::new(status, &body).into());
        }
        Ok(response)
    }

    /// `SHA256SUMS.txt` of the latest release.
    pub async fn manifest(&self) -> Result<String> {
        let url = format!("{}/latest/download/SHA256SUMS.txt", self.base);
        let read = async {
            let mut response = self.get(&url).await?;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > MAX_MANIFEST {
                    bail!("checksum list is larger than {MAX_MANIFEST} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(String::from_utf8_lossy(&body).into_owned())
        };
        tokio::time::timeout(MANIFEST_TIMEOUT, read)
            .await
            .context("timed out")?
    }

    /// Download `asset` of release `version` to `dest`, reporting
    /// `(received, total)` as it goes.
    ///
    /// `dest` exists afterwards only if every byte arrived and the SHA-256
    /// matches the manifest's; a partial or wrong file is removed.
    pub async fn download(
        &self,
        version: Version,
        asset: &Asset,
        dest: &Path,
        mut progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<()> {
        let url = format!("{}/download/v{version}/{}", self.base, asset.name);
        let part = dest.with_extension("part");
        let result = async {
            let response = self.get(&url).await?;
            let total = response.content_length();
            if total.is_some_and(|n| n > MAX_DOWNLOAD) {
                bail!("{} is larger than expected", asset.name);
            }
            let mut file = tokio::fs::File::create(&part)
                .await
                .with_context(|| format!("create {}", part.display()))?;
            let mut hash = Sha256::new();
            let mut received = 0u64;
            let mut body = response.bytes_stream();
            progress(0, total);
            loop {
                let chunk = tokio::time::timeout(IDLE_TIMEOUT, body.next())
                    .await
                    .context("the download stalled")?;
                let Some(chunk) = chunk else { break };
                let chunk = chunk.context("the download was interrupted")?;
                received += chunk.len() as u64;
                if received > MAX_DOWNLOAD {
                    bail!("{} is larger than expected", asset.name);
                }
                hash.update(&chunk);
                file.write_all(&chunk).await?;
                progress(received, total);
            }
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            if total.is_some_and(|n| n != received) {
                bail!("the download was cut short ({received} of {total:?} bytes)");
            }
            let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if actual != asset.sha256 {
                tracing::warn!(op = "update_download", file = %asset.name,
                    expected = %asset.sha256, %actual, "checksum mismatch");
                bail!(
                    "checksum mismatch: the downloaded {} is not the published file",
                    asset.name
                );
            }
            tokio::fs::rename(&part, dest).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(request line prefix, status line, body, declared length)`; a
    /// declared length above the body's cuts the response short.
    type Exchange = (&'static str, &'static str, Vec<u8>, Option<usize>);

    fn serve(script: Vec<Exchange>) -> (UpdateFeed, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (path, status, body, declared) in script {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let headers = String::from_utf8(request).unwrap();
                assert!(headers.starts_with(path), "{headers}");
                seen.push(headers);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    declared.unwrap_or(body.len())
                );
                let _ = stream.write_all(&body);
            }
            seen
        });
        (
            UpdateFeed::new(&format!("http://{address}/releases/")).unwrap(),
            server,
        )
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn asset(body: &[u8]) -> Asset {
        Asset {
            name: "ocdesk-9.9.9-linux-x86_64.AppImage".into(),
            sha256: sha256(body),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ocdesk-feed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const V: &str = "9.9.9";
    const DOWNLOAD: &str = "GET /releases/download/v9.9.9/ocdesk-9.9.9-linux-x86_64.AppImage ";

    #[test]
    fn only_https_or_loopback_feeds_are_accepted() {
        assert!(UpdateFeed::new(RELEASES).is_ok());
        assert!(UpdateFeed::new("http://127.0.0.1:1/releases").is_ok());
        assert!(UpdateFeed::new("http://localhost:1/releases").is_ok());
        assert!(UpdateFeed::new("http://[::1]:1/releases").is_ok());
        for bad in [
            "http://github.com/bmz1/ocdesk/releases",
            "http://127.0.0.1.example.com/releases",
            "ftp://127.0.0.1/releases",
            "file:///tmp/releases",
            "not a url",
        ] {
            assert!(UpdateFeed::new(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn manifest_is_read_from_the_latest_release_without_credentials() {
        let (feed, server) = serve(vec![(
            "GET /releases/latest/download/SHA256SUMS.txt ",
            "200 OK",
            b"abc  file\n".to_vec(),
            None,
        )]);
        assert_eq!(feed.manifest().await.unwrap(), "abc  file\n");
        let request = server.join().unwrap().remove(0).to_lowercase();
        assert!(request.contains(concat!("user-agent: ocdesk/", env!("CARGO_PKG_VERSION"))));
        assert!(!request.contains("authorization") && !request.contains("cookie"));
    }

    #[tokio::test]
    async fn manifest_failures_keep_their_status_and_oversized_lists_are_refused() {
        let (feed, server) = serve(vec![
            (
                "GET /releases/latest/",
                "404 Not Found",
                b"Not Found".to_vec(),
                None,
            ),
            (
                "GET /releases/latest/",
                "200 OK",
                vec![b'a'; MAX_MANIFEST + 1],
                None,
            ),
        ]);
        let missing = feed.manifest().await.unwrap_err();
        assert!(HttpError::is_not_found(&missing), "{missing:#}");
        let huge = feed.manifest().await.unwrap_err();
        assert!(format!("{huge:#}").contains("larger than"), "{huge:#}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_verified_download_lands_at_dest_and_reports_progress() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let (feed, server) = serve(vec![(DOWNLOAD, "200 OK", body.clone(), None)]);
        let dir = scratch("ok");
        let dest = dir.join("update.AppImage");
        let mut reports = Vec::new();
        feed.download(
            Version::parse(V).unwrap(),
            &asset(&body),
            &dest,
            |got, of| reports.push((got, of)),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let total = Some(body.len() as u64);
        assert_eq!(reports.first(), Some(&(0, total)));
        assert_eq!(reports.last(), Some(&(body.len() as u64, total)));
        assert!(reports.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no .part left");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_wrong_short_or_missing_download_leaves_no_file() {
        let body = b"not the released file".to_vec();
        let expected = asset(b"the released file");
        let (feed, server) = serve(vec![
            (DOWNLOAD, "200 OK", body.clone(), None),
            (DOWNLOAD, "200 OK", body.clone(), Some(body.len() + 10)),
            (DOWNLOAD, "404 Not Found", Vec::new(), None),
        ]);
        let dir = scratch("bad");
        let dest = dir.join("update.AppImage");
        let version = Version::parse(V).unwrap();
        let mismatch = feed
            .download(version, &expected, &dest, |_, _| {})
            .await
            .unwrap_err();
        assert!(
            format!("{mismatch:#}").contains("checksum mismatch"),
            "{mismatch:#}"
        );
        // Right checksum, but the connection closed before the declared end.
        let cut = feed
            .download(version, &asset(&body), &dest, |_, _| {})
            .await
            .unwrap_err();
        assert!(!format!("{cut:#}").contains("checksum"), "{cut:#}");
        let missing = feed
            .download(version, &expected, &dest, |_, _| {})
            .await
            .unwrap_err();
        assert!(HttpError::is_not_found(&missing));
        server.join().unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The real feed, over TLS and GitHub's redirects. Needs the network:
    /// `cargo test --locked -- --ignored live_feed`.
    #[tokio::test]
    #[ignore = "contacts github.com"]
    async fn live_feed_names_a_release_with_a_file_for_every_install_kind() {
        use crate::domain::update::{Package, Target, newer_release};
        let manifest = UpdateFeed::new(RELEASES).unwrap().manifest().await.unwrap();
        let oldest = Version::parse("0.0.0").unwrap();
        for (package, arch) in [
            (Package::AppImage, "x86_64"),
            (Package::Tarball, "x86_64"),
            (Package::MacApp, "aarch64"),
        ] {
            let target = Target {
                package,
                arch: arch.into(),
            };
            let release = newer_release(&manifest, oldest, Some(&target))
                .unwrap()
                .unwrap();
            assert!(release.asset.is_some(), "{package:?} in:\n{manifest}");
        }
    }

    #[test]
    fn release_page_is_the_tagged_github_release() {
        assert_eq!(
            release_page(Version::parse("1.2.3").unwrap()),
            "https://github.com/bmz1/ocdesk/releases/tag/v1.2.3"
        );
    }
}
