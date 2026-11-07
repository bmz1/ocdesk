//! Thin wrapper over `opencode-codes`. Everything network-facing goes through
//! here, and every future returned must run on the Tokio runtime (see
//! [`Runtime`]), never on the GPUI foreground executor.

pub mod events;

use std::time::Duration;

use anyhow::{Result, bail};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use opencode_codes::OpencodeClient;
use opencode_codes::http::BasicAuth;
use opencode_codes::protocol_generated::types as oc;
use reqwest::Method;

use crate::sidecar::{Sidecar, USERNAME};
pub use events::Event;

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

/// The server sends `server.heartbeat` about every 10 s; silence for this
/// long means the connection is dead even if TCP hasn't noticed.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_BACKOFF_MIN: Duration = Duration::from_millis(250);
const SSE_BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Api {
    client: OpencodeClient,
}

impl Api {
    pub fn new(sidecar: &Sidecar) -> Result<Self> {
        let client = OpencodeClient::builder()
            .base_url(sidecar.base_url.clone())
            .auth(BasicAuth::new(USERNAME, sidecar.password.clone()))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client })
    }

    /// Root sessions only (sub-agent children are hidden).
    pub async fn list_sessions(&self) -> Result<Vec<oc::Session>> {
        Ok(self
            .client
            .request(Method::GET, "/session?roots=true", None)
            .await?)
    }

    pub async fn create_session(&self) -> Result<oc::Session> {
        let params: oc::SessionCreateParams = serde_json::from_value(serde_json::json!({}))?;
        Ok(self.client.create_session(&params).await?)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let path = format!("/session/{}", urlencode(session_id));
        Ok(self.client.request(Method::DELETE, &path, None).await?)
    }

    pub async fn rename_session(&self, session_id: &str, title: &str) -> Result<oc::Session> {
        let path = format!("/session/{}", urlencode(session_id));
        let body = serde_json::json!({ "title": title });
        Ok(self
            .client
            .request(Method::PATCH, &path, Some(body))
            .await?)
    }

    pub async fn prompt_text(&self, session_id: &str, text: &str) -> Result<()> {
        let params: oc::PromptAsyncParams = serde_json::from_value(serde_json::json!({
            "parts": [{ "type": "text", "text": text }]
        }))?;
        Ok(self.client.prompt_async(session_id, &params).await?)
    }

    pub async fn messages(&self, session_id: &str) -> Result<Vec<oc::MessageWithParts>> {
        Ok(self.client.list_messages(session_id).await?)
    }

    pub async fn abort(&self, session_id: &str) -> Result<bool> {
        Ok(self.client.abort(session_id).await?)
    }

    /// Pump `GET /event` forever, forwarding normalized events.
    ///
    /// We read SSE ourselves instead of using `opencode_codes::EventStream`
    /// because that stream retries silently, so the UI could never show a
    /// disconnected state. Here every (re)connect yields [`Event::Connected`]
    /// and every drop yields [`Event::Disconnected`], then reconnects with
    /// exponential backoff. Returns when the receiver is dropped; the caller
    /// aborts the task to stop it early.
    pub async fn run_events(&self, tx: UnboundedSender<Event>) {
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
            if tx.unbounded_send(Event::Disconnected).is_err() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(SSE_BACKOFF_MAX);
        }
    }

    async fn stream_once(&self, tx: &UnboundedSender<Event>) -> Result<StreamEnd> {
        let resp = self
            .client
            .transport()
            .event_request()
            .header("accept", "text/event-stream")
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("GET /event returned {}", resp.status());
        }
        tracing::info!(target: "sse", "connected");
        if tx.unbounded_send(Event::Connected).is_err() {
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
                tracing::debug!(target: "sse", "{data}");
                let Some(ev) = Event::from_json(&data) else {
                    tracing::warn!(target: "sse", "undecodable frame: {data}");
                    continue;
                };
                if tx.unbounded_send(ev).is_err() {
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

fn urlencode(s: &str) -> String {
    // Session ids are `ses_` + base62; anything else is a bug upstream.
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
