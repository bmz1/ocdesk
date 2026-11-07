//! The only place that talks HTTP. Every future here must run on the Tokio
//! runtime (see [`Runtime`]), never on the GPUI foreground executor.
//!
//! ocdesk targets opencode's **v2** API (`/api/*`, see
//! `docs/protocol-notes.md`). We use a thin reqwest client instead of
//! `opencode-codes`, whose client wraps the v1 routes.

pub mod events;
pub mod model;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use reqwest::Method;
use serde_json::{Value, json};

use crate::sidecar::{Sidecar, USERNAME};
pub use events::{Envelope, Event};
pub use model::{Message, SessionInfo};

/// Dedicated Tokio runtime for reqwest/SSE, on its own threads.
pub struct Runtime(tokio::runtime::Runtime);

impl Runtime {
    pub fn new() -> Result<Self> {
        Ok(Self(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("ocdesk-net")
                .enable_all()
                .build()?,
        ))
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.0.handle().clone()
    }
}

/// The server sends `server.heartbeat` about every 10-15 s; silence for this
/// long means the connection is dead even if TCP hasn't noticed.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_BACKOFF_MIN: Duration = Duration::from_millis(250);
const SSE_BACKOFF_MAX: Duration = Duration::from_secs(5);
const PAGE: usize = 200;

/// Thin client over opencode's **v2** HTTP API (`/api/*`). The few
/// operations v2 doesn't expose yet (rename, delete, legacy history) use the
/// v1 routes; each is marked `v1`.
#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
    password: String,
}

impl Api {
    pub fn new(sidecar: &Sidecar) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            base: sidecar.base_url.clone(),
            password: sidecar.password.clone(),
        })
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .basic_auth(USERNAME, Some(&self.password))
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<Value> {
        let resp = rb.timeout(Duration::from_secs(30)).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// All sessions in every location, newest first (`GET /api/session`).
    /// Sub-agent sessions are dropped.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q = vec![("limit", PAGE.to_string()), ("order", "desc".to_string())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            }
            let v = self
                .send(self.req(Method::GET, "/api/session").query(&q))
                .await?;
            let page = v["data"].as_array().cloned().unwrap_or_default();
            let n = page.len();
            out.extend(
                page.iter()
                    .filter_map(SessionInfo::from_json)
                    .filter(|s| s.parent_id.is_none()),
            );
            cursor = v
                .pointer("/cursor/next")
                .and_then(Value::as_str)
                .map(str::to_string);
            if n < PAGE || cursor.is_none() {
                return Ok(out);
            }
        }
    }

    /// `POST /api/session` pinned to `directory`.
    pub async fn create_session(&self, directory: &Path) -> Result<SessionInfo> {
        let body = json!({ "location": { "directory": directory } });
        let v = self
            .send(self.req(Method::POST, "/api/session").json(&body))
            .await?;
        SessionInfo::from_json(&v["data"])
            .ok_or_else(|| anyhow!("unexpected session response: {v}"))
    }

    /// `POST /api/session/{id}/prompt`. Returns once the input is admitted;
    /// the turn itself is observed on the event stream.
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<()> {
        let body = json!({ "prompt": { "text": text } });
        self.send(
            self.req(
                Method::POST,
                &format!("/api/session/{}/prompt", id(session_id)),
            )
            .json(&body),
        )
        .await?;
        Ok(())
    }

    /// Full history in chronological order (`GET /api/session/{id}/message`).
    /// Falls back to the v1 history for sessions created through the v1 API,
    /// which have no v2 projection; the bool says whether that happened.
    pub async fn messages(
        &self,
        session_id: &str,
        directory: &Path,
    ) -> Result<(Vec<Message>, bool)> {
        let mut raw = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q = vec![("limit", PAGE.to_string()), ("order", "asc".to_string())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            }
            let path = format!("/api/session/{}/message", id(session_id));
            let v = self.send(self.req(Method::GET, &path).query(&q)).await?;
            let page = v["data"].as_array().cloned().unwrap_or_default();
            let n = page.len();
            raw.extend(page);
            cursor = v
                .pointer("/cursor/next")
                .and_then(Value::as_str)
                .map(str::to_string);
            if n < PAGE || cursor.is_none() {
                break;
            }
        }
        if raw.is_empty() {
            // v1 (legacy) session?
            let path = format!("/session/{}/message", id(session_id));
            let v = self
                .send(
                    self.req(Method::GET, &path)
                        .query(&[("directory", directory)]),
                )
                .await?;
            let legacy: Vec<Message> = v
                .as_array()
                .map(|a| a.iter().filter_map(model::message_from_v1_json).collect())
                .unwrap_or_default();
            if !legacy.is_empty() {
                return Ok((legacy, true));
            }
        }
        let mut msgs: Vec<Message> = raw.iter().filter_map(Message::from_v2_json).collect();
        msgs.sort_by(|a, b| a.id().cmp(b.id()));
        Ok((msgs, false))
    }

    /// `POST /api/session/{id}/interrupt`. The turn ends with
    /// `session.next.step.failed` ("Provider turn interrupted").
    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        self.send(self.req(
            Method::POST,
            &format!("/api/session/{}/interrupt", id(session_id)),
        ))
        .await?;
        Ok(())
    }

    /// Sessions currently running (`GET /api/session/active`). v2 has no
    /// idle/busy event, so this seeds busy state after every (re)connect.
    pub async fn active(&self) -> Result<Vec<String>> {
        let v = self
            .send(self.req(Method::GET, "/api/session/active"))
            .await?;
        Ok(v["data"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Validate a folder and resolve its project (`GET /api/location`).
    pub async fn location(&self, directory: &Path) -> Result<PathBuf> {
        let v = self
            .send(
                self.req(Method::GET, "/api/location")
                    .query(&[("location[directory]", directory)]),
            )
            .await?;
        Ok(v["directory"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.to_path_buf()))
    }

    /// v1 `PATCH /session/{id}`: v2 has no rename yet.
    pub async fn rename_session(
        &self,
        session_id: &str,
        directory: &Path,
        title: &str,
    ) -> Result<()> {
        let path = format!("/session/{}", id(session_id));
        self.send(
            self.req(Method::PATCH, &path)
                .query(&[("directory", directory)])
                .json(&json!({ "title": title })),
        )
        .await?;
        Ok(())
    }

    /// v1 `DELETE /session/{id}`: v2 has no delete yet.
    pub async fn delete_session(&self, session_id: &str, directory: &Path) -> Result<()> {
        let path = format!("/session/{}", id(session_id));
        self.send(
            self.req(Method::DELETE, &path)
                .query(&[("directory", directory)]),
        )
        .await?;
        Ok(())
    }

    /// Pump `GET /api/event` (all locations) forever, forwarding decoded
    /// envelopes. Every (re)connect yields [`Event::Connected`] and every
    /// drop yields [`Event::Disconnected`], then reconnects with exponential
    /// backoff. Returns when the receiver is dropped; the caller aborts the
    /// task to stop it early.
    pub async fn run_events(&self, tx: UnboundedSender<Envelope>) {
        let mut backoff = SSE_BACKOFF_MIN;
        loop {
            match self.stream_once(&tx).await {
                Ok(StreamEnd::ReceiverGone) => return,
                Ok(StreamEnd::Dropped) => {
                    backoff = SSE_BACKOFF_MIN;
                    tracing::warn!(target: "sse", "event stream dropped");
                }
                Err(e) => tracing::warn!(target: "sse", "event stream connect failed: {e:#}"),
            }
            if tx
                .unbounded_send(Envelope::bare(Event::Disconnected))
                .is_err()
            {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(SSE_BACKOFF_MAX);
        }
    }

    async fn stream_once(&self, tx: &UnboundedSender<Envelope>) -> Result<StreamEnd> {
        let resp = self
            .req(Method::GET, "/api/event")
            .header("accept", "text/event-stream")
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("GET /api/event returned {}", resp.status());
        }
        tracing::info!(target: "sse", "connected");
        if tx.unbounded_send(Envelope::bare(Event::Connected)).is_err() {
            return Ok(StreamEnd::ReceiverGone);
        }
        let mut body = resp.bytes_stream();
        let mut parser = SseParser::default();
        loop {
            let chunk = match tokio::time::timeout(SSE_IDLE_TIMEOUT, body.next()).await {
                Err(_) => {
                    tracing::warn!(target: "sse", "no data for {SSE_IDLE_TIMEOUT:?}");
                    return Ok(StreamEnd::Dropped);
                }
                Ok(None) => return Ok(StreamEnd::Dropped),
                Ok(Some(Err(e))) => {
                    tracing::warn!(target: "sse", "read error: {e}");
                    return Ok(StreamEnd::Dropped);
                }
                Ok(Some(Ok(chunk))) => chunk,
            };
            for data in parser.push(&chunk) {
                let Some(env) = Envelope::from_json(&data) else {
                    tracing::warn!(target: "sse", "undecodable frame: {data}");
                    continue;
                };
                if !matches!(&env.event, Event::Other(t) if t == "plugin.added" || t == "server.heartbeat")
                {
                    tracing::debug!(target: "sse", "{data}");
                }
                if tx.unbounded_send(env).is_err() {
                    return Ok(StreamEnd::ReceiverGone);
                }
            }
        }
    }
}

enum StreamEnd {
    Dropped,
    ReceiverGone,
}

/// Session ids are `ses_` + base62; strip anything else before putting one
/// in a path.
fn id(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Minimal SSE framing: collects `data:` lines until a blank line.
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    data: String,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data));
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // `event:`, `id:`, `retry:` and comments are not used by opencode.
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::SseParser;

    #[test]
    fn sse_frames_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\":").is_empty());
        assert_eq!(
            p.push(b"1}\n\ndata: {\"b\":2}\r\n\r\n: comment\n"),
            ["{\"a\":1}", "{\"b\":2}"]
        );
        assert!(p.push(b"data: x\n").is_empty());
        assert_eq!(p.push(b"data: y\n\n"), ["x\ny"]);
    }
}
