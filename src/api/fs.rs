//! Folder file access for the editor tabs: list, read (with ETag and Range),
//! write and watch. Paths are relative to the folder and must already be
//! confined with [`crate::files::rel_path`].

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use reqwest::{Method, StatusCode, header};
use serde_json::json;

use super::Api;
use super::model::FsEntry;
use crate::sidecar::USERNAME;

const FS_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of [`Api::read_file`].
#[derive(Clone, Debug, PartialEq)]
pub enum ReadResult {
    /// `If-None-Match` matched: the file is unchanged.
    NotModified,
    /// The file is gone (404).
    Missing,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        /// Full size of the file (more than `bytes.len()` when truncated).
        total: u64,
    },
}

/// `bytes 0-99/1234` → 1234.
fn content_range_total(v: &str) -> Option<u64> {
    v.rsplit_once('/')?.1.trim().parse().ok()
}

impl Api {
    /// `self.base` + `/api/fs/read/<segments>`, each segment percent-encoded.
    fn read_url(&self, rel: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid base url"))?
            .pop_if_empty()
            .extend(["api", "fs", "read"])
            .extend(rel.split('/'));
        Ok(url)
    }

    /// Direct children of `path` (`""` is the folder itself), directories
    /// first, then by name (`GET /api/fs/list`). Entry paths come back
    /// relative to the folder, without a trailing slash.
    pub async fn list_dir(&self, directory: &Path, path: &str) -> Result<Vec<FsEntry>> {
        let mut rb = self
            .req(Method::GET, "/api/fs/list")
            .query(&[("location[directory]", directory)]);
        if !path.is_empty() {
            rb = rb.query(&[("path", path)]);
        }
        let v = self.send(rb).await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid list response"))?;
        let mut out: Vec<FsEntry> = data
            .iter()
            .filter_map(|e| {
                let raw = e["path"].as_str()?.trim_end_matches('/');
                let name = raw.rsplit('/').next().filter(|n| !n.is_empty())?;
                if name == "." || name == ".." {
                    return None;
                }
                // Defensive: some versions return bare names for subdirectories.
                let full = if path.is_empty() || raw.starts_with(&format!("{path}/")) {
                    raw.to_string()
                } else {
                    format!("{path}/{name}")
                };
                Some(FsEntry {
                    path: full,
                    is_dir: e["type"].as_str() == Some("directory"),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
        });
        Ok(out)
    }

    /// Read up to `limit` bytes of a file (`GET /api/fs/read/*`). With
    /// `etag`, an unchanged file answers [`ReadResult::NotModified`].
    pub async fn read_file(
        &self,
        directory: &Path,
        rel: &str,
        etag: Option<&str>,
        limit: u64,
    ) -> Result<ReadResult> {
        let url = self.read_url(rel)?;
        let build = |range: bool| {
            let mut rb = self
                .http
                .request(Method::GET, url.clone())
                .basic_auth(USERNAME, Some(&self.password))
                .query(&[("location[directory]", directory)])
                .timeout(FS_TIMEOUT);
            if range {
                rb = rb.header(
                    header::RANGE,
                    format!("bytes=0-{}", limit.saturating_sub(1)),
                );
            }
            if let Some(tag) = etag {
                rb = rb.header(header::IF_NONE_MATCH, tag);
            }
            rb
        };
        let mut resp = build(true).send().await?;
        // An empty file can't satisfy any range: ask for all of it.
        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            resp = build(false).send().await?;
        }
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(ReadResult::NotModified);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(ReadResult::Missing);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let range_total = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_total);
        let mut bytes = resp.bytes().await?.to_vec();
        let total = match (status, range_total) {
            (StatusCode::PARTIAL_CONTENT, Some(t)) => t,
            _ => bytes.len() as u64,
        };
        // A server that ignored Range sent everything: keep `limit`.
        bytes.truncate(limit as usize);
        Ok(ReadResult::Body { bytes, etag, total })
    }

    /// Replace a file's contents (`POST /api/experimental/fs/write`).
    pub async fn write_file(&self, directory: &Path, rel: &str, bytes: Vec<u8>) -> Result<()> {
        self.send(
            self.req(Method::POST, "/api/experimental/fs/write")
                .query(&[("location[directory]", directory)])
                .query(&[("path", rel)])
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(bytes),
        )
        .await?;
        Ok(())
    }

    /// Start (or renew, for 24 h) or stop watching one file; changes arrive
    /// as `filesystem.changed` (`/api/experimental/fs/watch`).
    pub async fn watch_file(&self, directory: &Path, rel: &str, on: bool) -> Result<()> {
        let method = if on { Method::POST } else { Method::DELETE };
        self.send(
            self.req(method, "/api/experimental/fs/watch")
                .query(&[("location[directory]", directory)])
                .json(&json!({ "path": rel })),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Api, ReadResult, content_range_total};

    /// One scripted reply per connection: (request line prefix, required
    /// header lines, expected body, raw response head, response body).
    type Exchange = (
        &'static str,
        &'static [&'static str],
        &'static str,
        &'static str,
        &'static str,
    );

    fn server(script: Vec<Exchange>) -> (Api, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for (line, needs, body, head, reply) in script {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let headers = String::from_utf8(request).unwrap();
                assert!(headers.starts_with(line), "{headers}");
                let lower = headers.to_lowercase();
                for need in needs {
                    assert!(lower.contains(&need.to_lowercase()), "{need} in {headers}");
                }
                let len: usize = lower
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                let mut payload = vec![0; len];
                stream.read_exact(&mut payload).unwrap();
                assert_eq!(String::from_utf8(payload).unwrap(), body, "{line}");
                write!(
                    stream,
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        let api = Api {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: format!("http://{address}"),
            password: "test".into(),
        };
        (api, handle)
    }

    #[::core::prelude::v1::test]
    fn parses_content_range() {
        assert_eq!(content_range_total("bytes 0-9/1234"), Some(1234));
        assert_eq!(content_range_total("bytes */0"), Some(0));
        assert_eq!(content_range_total("bytes 0-9/*"), None);
    }

    #[tokio::test]
    async fn fs_http_contract() {
        let dir = std::path::Path::new("/repo");
        let (api, server) = server(vec![
            (
                "GET /api/fs/list?location%5Bdirectory%5D=%2Frepo&path=src ",
                &[],
                "",
                "200 OK\r\nContent-Type: application/json",
                r#"{"location":{"directory":"/repo"},"data":[{"path":"src/b.rs","type":"file"},{"path":"src/a/","type":"directory"},{"path":"src/A.rs","type":"file"}]}"#,
            ),
            (
                "GET /api/fs/read/src/my%20file.rs?location%5Bdirectory%5D=%2Frepo ",
                &["range: bytes=0-9"],
                "",
                "206 Partial Content\r\nETag: \"v1\"\r\nContent-Range: bytes 0-9/42",
                "0123456789",
            ),
            (
                "GET /api/fs/read/src/my%20file.rs?",
                &["if-none-match: \"v1\""],
                "",
                "304 Not Modified",
                "",
            ),
            ("GET /api/fs/read/gone.rs?", &[], "", "404 Not Found", "{}"),
            (
                "POST /api/experimental/fs/write?location%5Bdirectory%5D=%2Frepo&path=src%2Fa.rs ",
                &["content-type: application/octet-stream"],
                "hello\r\n",
                "200 OK\r\nContent-Type: application/json",
                r#"{"location":{"directory":"/repo"},"data":{"path":"/repo/src/a.rs"}}"#,
            ),
            (
                "POST /api/experimental/fs/watch?location%5Bdirectory%5D=%2Frepo ",
                &[],
                r#"{"path":"src/a.rs"}"#,
                "204 No Content",
                "",
            ),
        ]);
        let list = api.list_dir(dir, "src").await.unwrap();
        let names: Vec<_> = list.iter().map(|e| (e.path.as_str(), e.is_dir)).collect();
        assert_eq!(
            names,
            [("src/a", true), ("src/A.rs", false), ("src/b.rs", false)]
        );
        assert_eq!(
            api.read_file(dir, "src/my file.rs", None, 10)
                .await
                .unwrap(),
            ReadResult::Body {
                bytes: b"0123456789".to_vec(),
                etag: Some("\"v1\"".into()),
                total: 42
            }
        );
        assert_eq!(
            api.read_file(dir, "src/my file.rs", Some("\"v1\""), 10)
                .await
                .unwrap(),
            ReadResult::NotModified
        );
        assert_eq!(
            api.read_file(dir, "gone.rs", None, 10).await.unwrap(),
            ReadResult::Missing
        );
        api.write_file(dir, "src/a.rs", b"hello\r\n".to_vec())
            .await
            .unwrap();
        api.watch_file(dir, "src/a.rs", true).await.unwrap();
        server.join().unwrap();
    }
}
