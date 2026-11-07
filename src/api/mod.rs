//! The only place that talks HTTP. Every future here must run on the Tokio
//! runtime (see [`Runtime`]), never on the GPUI foreground executor.
//!
//! ocdesk speaks opencode 2.x's API (`/api/*`, see `docs/protocol-notes.md`)
//! through a thin reqwest client.

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

/// The server sends a `: heartbeat` comment every ~10 s; silence for this
/// long means the connection is dead even if TCP hasn't noticed.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_BACKOFF_MIN: Duration = Duration::from_millis(250);
const SSE_BACKOFF_MAX: Duration = Duration::from_secs(5);
const PAGE: usize = 200;

/// Thin client over opencode 2.x's HTTP API (`/api/*`).
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
            // `cursor` must not be combined with `order` (the API rejects it).
            let mut q = vec![("limit", PAGE.to_string())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            } else {
                q.push(("order", "desc".to_string()));
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
        let body = json!({ "text": text });
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
    pub async fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut raw = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            // The API rejects `cursor` combined with `order`, so `order` is
            // only sent for the first page.
            let mut q = vec![("limit", PAGE.to_string())];
            if cursor.is_none() {
                q.push(("order", "asc".to_string()));
            } else {
                q.push(("cursor", cursor.clone().unwrap()));
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
        let mut msgs: Vec<Message> = raw.iter().filter_map(Message::from_v2_json).collect();
        msgs.sort_by(|a, b| a.id().cmp(b.id()));
        Ok(msgs)
    }

    pub async fn permissions(&self, session_id: &str) -> Result<Vec<model::PermissionRequest>> {
        let path = format!("/api/session/{}/permission", id(session_id));
        let v = self.send(self.req(Method::GET, &path)).await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid permissions response"))?;
        data.iter()
            .map(|v| {
                model::PermissionRequest::from_json(v)
                    .ok_or_else(|| anyhow!("invalid permission request"))
            })
            .collect()
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: model::PermissionDecision,
    ) -> Result<()> {
        let path = format!(
            "/api/session/{}/permission/{}/reply",
            id(session_id),
            id(request_id)
        );
        self.send(
            self.req(Method::POST, &path)
                .json(&json!({"decision": decision.as_str()})),
        )
        .await?;
        Ok(())
    }

    /// Pending question forms for one session (`GET /api/session/{id}/form`).
    pub async fn forms(&self, session_id: &str) -> Result<Vec<model::FormRequest>> {
        let path = format!("/api/session/{}/form", id(session_id));
        let v = self.send(self.req(Method::GET, &path)).await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid forms response"))?;
        Ok(data
            .iter()
            .filter_map(model::FormRequest::from_json)
            .collect())
    }

    /// Answer a form (`POST /api/session/{id}/form/{form}/reply`, body
    /// `{"answer": {key: value}}` → 204).
    pub async fn reply_form(&self, session_id: &str, form_id: &str, answer: &Value) -> Result<()> {
        let path = format!("/api/session/{}/form/{}/reply", id(session_id), id(form_id));
        self.send(
            self.req(Method::POST, &path)
                .json(&json!({ "answer": answer })),
        )
        .await?;
        Ok(())
    }

    /// Cancel a form without answering (`DELETE /api/session/{id}/form/{form}`).
    pub async fn cancel_form(&self, session_id: &str, form_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/form/{}", id(session_id), id(form_id));
        self.send(self.req(Method::DELETE, &path)).await?;
        Ok(())
    }

    /// Latest turn, with bounded context; the server marks truncated patches with `more`.
    pub async fn diff(&self, session_id: &str) -> Result<Vec<model::FileDiff>> {
        let path = format!("/api/session/{}/diff", id(session_id));
        let v = self
            .send(self.req(Method::GET, &path).query(&[("context", "3")]))
            .await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid diff response"))?;
        data.iter()
            .map(|v| model::FileDiff::from_json(v).ok_or_else(|| anyhow!("invalid file diff")))
            .collect()
    }

    /// `POST /api/session/{id}/interrupt`. The turn ends with
    /// `session.execution.interrupted {reason: "user"}`.
    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        self.send(self.req(
            Method::POST,
            &format!("/api/session/{}/interrupt", id(session_id)),
        ))
        .await?;
        Ok(())
    }

    /// Sessions currently running (`GET /api/session/active`); seeds busy
    /// state after every (re)connect.
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

    /// `PATCH /api/session/{id}` → 204; confirmed by `session.renamed`.
    pub async fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        let path = format!("/api/session/{}", id(session_id));
        self.send(
            self.req(Method::PATCH, &path)
                .json(&json!({ "title": title })),
        )
        .await?;
        Ok(())
    }

    /// `DELETE /api/session/{id}` → 204; confirmed by `session.deleted`.
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{}", id(session_id));
        self.send(self.req(Method::DELETE, &path)).await?;
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
    use super::{Api, SseParser};
    use crate::api::model::PermissionDecision;

    /// Exercise the actual reqwest wire requests, including empty 204 replies.
    #[tokio::test]
    async fn permission_and_diff_http_contract() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (path, body, response, status) in [
                (
                    "GET /api/session/ses_a/permission ",
                    "",
                    r#"{"data":[{"id":"perm_a","sessionID":"ses_a","action":"shell","resources":["ls"]}]}"#,
                    "200 OK",
                ),
                (
                    "POST /api/session/ses_a/permission/perm_a/reply ",
                    r#"{"decision":"once"}"#,
                    "",
                    "204 No Content",
                ),
                (
                    "POST /api/session/ses_a/permission/perm_a/reply ",
                    r#"{"decision":"always"}"#,
                    "",
                    "204 No Content",
                ),
                (
                    "POST /api/session/ses_a/permission/perm_a/reply ",
                    r#"{"decision":"reject"}"#,
                    "denied",
                    "500 Internal Server Error",
                ),
                (
                    "GET /api/session/ses_a/diff?context=3 ",
                    "",
                    r#"{"data":[{"file":"a.rs","patch":"+new","status":"added","additions":1,"deletions":0}]}"#,
                    "200 OK",
                ),
                (
                    "GET /api/session/ses_a/form ",
                    "",
                    r#"{"data":[{"id":"frm_a","sessionID":"ses_a","title":"Questions","fields":[{"key":"q0","type":"string","description":"Pick one","options":[{"value":"y","label":"Yes"}],"custom":true}]}]}"#,
                    "200 OK",
                ),
                (
                    "POST /api/session/ses_a/form/frm_a/reply ",
                    r#"{"answer":{"q0":"y"}}"#,
                    "",
                    "204 No Content",
                ),
                (
                    "DELETE /api/session/ses_a/form/frm_a ",
                    "",
                    "",
                    "204 No Content",
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let headers = String::from_utf8(request).unwrap();
                assert!(headers.starts_with(path), "{headers}");
                assert!(headers.to_lowercase().contains("authorization: basic "));
                let len: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                let mut payload = vec![0; len];
                stream.read_exact(&mut payload).unwrap();
                assert_eq!(String::from_utf8(payload).unwrap(), body);
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let api = Api {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: format!("http://{address}"),
            password: "test".into(),
        };
        assert_eq!(api.permissions("ses_a").await.unwrap()[0].id, "perm_a");
        api.reply_permission("ses_a", "perm_a", PermissionDecision::Once)
            .await
            .unwrap();
        api.reply_permission("ses_a", "perm_a", PermissionDecision::Always)
            .await
            .unwrap();
        assert!(
            api.reply_permission("ses_a", "perm_a", PermissionDecision::Reject)
                .await
                .is_err()
        );
        assert_eq!(api.diff("ses_a").await.unwrap()[0].file, "a.rs");
        let form = &api.forms("ses_a").await.unwrap()[0];
        assert_eq!(form.id, "frm_a");
        assert!(form.fields[0].custom);
        api.reply_form("ses_a", "frm_a", &serde_json::json!({"q0": "y"}))
            .await
            .unwrap();
        api.cancel_form("ses_a", "frm_a").await.unwrap();
        server.join().unwrap();
    }

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
