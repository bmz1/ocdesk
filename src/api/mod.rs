//! Thin wrapper over `opencode-codes`. Everything network-facing goes through
//! here, and every future returned must run on the Tokio runtime (see
//! [`Runtime`]), never on the GPUI foreground executor.

pub mod events;

use std::time::Duration;

use anyhow::Result;
use futures::channel::mpsc::UnboundedSender;
use opencode_codes::http::BasicAuth;
use opencode_codes::protocol_generated::types as oc;
use opencode_codes::{OpencodeClient, RetryConfig};

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

    pub async fn create_session(&self, title: Option<String>) -> Result<oc::Session> {
        let params: oc::SessionCreateParams =
            serde_json::from_value(serde_json::json!({ "title": title }))?;
        Ok(self.client.create_session(&params).await?)
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

    /// Pump `GET /event` forever, forwarding normalized events. Reconnects with
    /// backoff (handled by the SSE layer); every reconnect yields
    /// [`Event::Connected`]. Returns when the receiver is dropped.
    pub async fn run_events(&self, tx: UnboundedSender<Event>) -> Result<()> {
        let retry = RetryConfig {
            initial_interval: Duration::from_millis(250),
            max_interval: Duration::from_secs(5),
            factor: 2.0,
            max_retries: None,
        };
        let mut stream = self.client.event_stream(retry)?;
        while let Some(item) = stream.next().await {
            let ev = match item {
                Ok(ev) => {
                    log_raw(&ev);
                    Event::from_stream(ev)
                }
                Err(e) => {
                    tracing::warn!("event stream error: {e}");
                    continue;
                }
            };
            if tx.unbounded_send(ev).is_err() {
                break;
            }
        }
        Ok(())
    }
}

fn log_raw(ev: &opencode_codes::StreamEvent) {
    use opencode_codes::StreamEvent as S;
    match ev {
        S::Connected => tracing::info!(target: "sse", "connected"),
        S::Event(e) => tracing::debug!(
            target: "sse",
            "{}",
            serde_json::to_string(e).unwrap_or_else(|err| format!("<unserializable: {err}>"))
        ),
        S::Unknown(v) => tracing::debug!(target: "sse", "unknown: {v}"),
        other => tracing::debug!(target: "sse", "{other:?}"),
    }
}
