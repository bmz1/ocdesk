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
    pub async fn prompt(&self, session_id: &str, input: &model::PromptInput) -> Result<()> {
        self.send(
            self.req(
                Method::POST,
                &format!("/api/session/{}/prompt", id(session_id)),
            )
            .json(&input.to_json()),
        )
        .await?;
        Ok(())
    }

    /// Enabled models for a folder (`GET /api/model`), chat models only.
    pub async fn models(&self, directory: &Path) -> Result<Vec<model::ModelInfo>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/model")
                    .query(&[("location[directory]", directory)])
                    .query(&[("types", "chat")]),
            )
            .await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid models response"))?;
        Ok(data
            .iter()
            .filter_map(model::ModelInfo::from_json)
            .collect())
    }

    /// The folder's configured default model (`GET /api/model/default`).
    pub async fn default_model(&self, directory: &Path) -> Result<Option<model::ModelRef>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/model/default")
                    .query(&[("location[directory]", directory)]),
            )
            .await?;
        Ok(model::ModelInfo::from_json(&v["data"]).map(|m| m.as_ref()))
    }

    /// Agents for a folder (`GET /api/agent`).
    pub async fn agents(&self, directory: &Path) -> Result<Vec<model::AgentInfo>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/agent")
                    .query(&[("location[directory]", directory)]),
            )
            .await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid agents response"))?;
        Ok(data
            .iter()
            .filter_map(model::AgentInfo::from_json)
            .collect())
    }

    /// Fuzzy file search in a folder (`GET /api/fs/find`).
    pub async fn find_files(
        &self,
        directory: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<model::FsEntry>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/fs/find")
                    .query(&[("location[directory]", directory)])
                    .query(&[("query", query), ("limit", &limit.to_string())]),
            )
            .await?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid find response"))?;
        Ok(data
            .iter()
            .filter_map(|e| {
                let path = e["path"].as_str()?;
                // "/" is the location root itself.
                (path != "/").then(|| model::FsEntry {
                    path: path.to_string(),
                    is_dir: e["type"].as_str() == Some("directory"),
                })
            })
            .collect())
    }

    /// `POST /api/session/{id}/model` → 204; confirmed by `session.model.selected`.
    pub async fn switch_model(&self, session_id: &str, model: &model::ModelRef) -> Result<()> {
        let path = format!("/api/session/{}/model", id(session_id));
        self.send(
            self.req(Method::POST, &path)
                .json(&json!({ "model": model.to_json() })),
        )
        .await?;
        Ok(())
    }

    /// `POST /api/session/{id}/agent` → 204; confirmed by `session.agent.selected`.
    pub async fn switch_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        let path = format!("/api/session/{}/agent", id(session_id));
        self.send(
            self.req(Method::POST, &path)
                .json(&json!({ "agent": agent })),
        )
        .await?;
        Ok(())
    }

    /// Full history in transcript order (`GET /api/session/{id}/message`).
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
        // Keep the server's order: it is delivery order, which differs from
        // id order once prompts are queued/steered (a queued prompt gets its
        // id when sent but lands after a later steer).
        Ok(raw.iter().filter_map(Message::from_v2_json).collect())
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

    // ── M7: inbox, commands, skills, MCP, shells, VCS, worktrees ─────────

    /// Pending prompts/compactions of a session (`GET …/inbox`), oldest first.
    pub async fn inbox(&self, session_id: &str) -> Result<Vec<model::InboxItem>> {
        let path = format!("/api/session/{}/inbox", id(session_id));
        let v = self.send(self.req(Method::GET, &path)).await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(|i| model::InboxItem::from_json(None, i))
            .collect())
    }

    /// Withdraw a pending item → 204 + `session.inbox.cancelled`.
    pub async fn cancel_inbox(&self, session_id: &str, item_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/inbox/{}", id(session_id), id(item_id));
        self.send(self.req(Method::DELETE, &path)).await?;
        Ok(())
    }

    /// Re-target a pending item → 204 + `session.inbox.delivery.changed`.
    pub async fn set_inbox_delivery(
        &self,
        session_id: &str,
        item_id: &str,
        delivery: model::Delivery,
    ) -> Result<()> {
        let path = format!("/api/session/{}/inbox/{}", id(session_id), id(item_id));
        self.send(
            self.req(Method::PATCH, &path)
                .json(&json!({ "delivery": delivery.as_str() })),
        )
        .await?;
        Ok(())
    }

    /// Summarize the context (`POST …/compact`); queued like a steer item.
    pub async fn compact(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/compact", id(session_id));
        self.send(self.req(Method::POST, &path).json(&json!({})))
            .await?;
        Ok(())
    }

    /// Server-side slash commands for a folder (`GET /api/command`).
    pub async fn commands(&self, directory: &Path) -> Result<Vec<model::CommandInfo>> {
        let v = self.get_in("/api/command", directory).await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(model::CommandInfo::from_json)
            .collect())
    }

    /// Run a command (`POST …/command {name, text}`); the expanded template
    /// becomes the user message.
    pub async fn run_command(
        &self,
        session_id: &str,
        name: &str,
        text: &str,
        delivery: Option<model::Delivery>,
    ) -> Result<()> {
        let path = format!("/api/session/{}/command", id(session_id));
        let mut body = json!({ "name": name, "text": text });
        if let Some(d) = delivery {
            body["delivery"] = d.as_str().into();
        }
        self.send(self.req(Method::POST, &path).json(&body)).await?;
        Ok(())
    }

    pub async fn skills(&self, directory: &Path) -> Result<Vec<model::SkillInfo>> {
        let v = self.get_in("/api/skill", directory).await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(model::SkillInfo::from_json)
            .collect())
    }

    pub async fn mcp_servers(&self, directory: &Path) -> Result<Vec<model::McpServer>> {
        let v = self.get_in("/api/mcp", directory).await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(model::McpServer::from_json)
            .collect())
    }

    /// `POST /api/experimental/mcp/{server}/connect|disconnect`.
    pub async fn mcp_set_connected(
        &self,
        directory: &Path,
        server: &str,
        connect: bool,
    ) -> Result<()> {
        let verb = if connect { "connect" } else { "disconnect" };
        let path = format!("/api/experimental/mcp/{}/{verb}", id(server));
        self.send(
            self.req(Method::POST, &path)
                .query(&[("location[directory]", directory)]),
        )
        .await?;
        Ok(())
    }

    /// Shell commands the server runs for a folder (`GET /api/shell`).
    pub async fn shells(&self, directory: &Path) -> Result<Vec<model::ShellInfo>> {
        let v = self.get_in("/api/shell", directory).await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(model::ShellInfo::from_json)
            .collect())
    }

    /// The tail of a shell's output (`GET /api/shell/{id}/output`).
    pub async fn shell_output(&self, directory: &Path, shell_id: &str) -> Result<String> {
        let path = format!("/api/shell/{}/output", id(shell_id));
        let v = self.get_in(&path, directory).await?;
        Ok(v["data"]["output"].as_str().unwrap_or_default().to_string())
    }

    /// Kill a shell (`DELETE /api/shell/{id}`) → `shell.exited {status: killed}`.
    pub async fn kill_shell(&self, directory: &Path, shell_id: &str) -> Result<()> {
        let path = format!("/api/shell/{}", id(shell_id));
        self.send(
            self.req(Method::DELETE, &path)
                .query(&[("location[directory]", directory)]),
        )
        .await?;
        Ok(())
    }

    /// Child (subagent) sessions of a session, newest first.
    pub async fn children(&self, session_id: &str) -> Result<Vec<SessionInfo>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/session")
                    .query(&[("parentID", session_id), ("limit", "100")]),
            )
            .await?;
        Ok(array(&v["data"])?
            .iter()
            .filter_map(SessionInfo::from_json)
            .collect())
    }

    pub async fn vcs(&self, directory: &Path) -> Result<model::VcsInfo> {
        let v = self.get_in("/api/vcs", directory).await?;
        Ok(model::VcsInfo::from_json(&v["data"]))
    }

    /// The project a folder belongs to (`GET /api/location` → `project.id`).
    pub async fn project_id(&self, directory: &Path) -> Result<String> {
        let v = self.get_in("/api/location", directory).await?;
        v.pointer("/project/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{} has no project", directory.display()))
    }

    /// Worktree directories of a project (the main checkout included).
    pub async fn worktrees(&self, project_id: &str) -> Result<Vec<PathBuf>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/worktree")
                    .query(&[("projectID", project_id)]),
            )
            .await?;
        Ok(array(&v)?
            .iter()
            .filter_map(|w| w["directory"].as_str().map(PathBuf::from))
            .collect())
    }

    /// `POST /api/worktree {projectID, name}` → the new checkout's directory.
    pub async fn create_worktree(&self, project_id: &str, name: &str) -> Result<PathBuf> {
        let v = self
            .send(
                self.req(Method::POST, "/api/worktree")
                    .json(&json!({ "projectID": project_id, "name": name })),
            )
            .await?;
        v["directory"]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("unexpected worktree response: {v}"))
    }

    /// `DELETE /api/worktree`. Without `force`, a worktree with changes is
    /// refused with [`WorktreeDirty`] (the server's `forceRequired: true`).
    pub async fn remove_worktree(
        &self,
        project_id: &str,
        directory: &Path,
        force: bool,
    ) -> Result<()> {
        let resp = self
            .req(Method::DELETE, "/api/worktree")
            .json(&json!({ "projectID": project_id, "directory": directory, "force": force }))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if body.pointer("/data/forceRequired") == Some(&Value::Bool(true)) {
            return Err(WorktreeDirty.into());
        }
        let msg = body
            .pointer("/data/message")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("HTTP {status}: {msg}")
    }

    /// Re-pin a session to another directory (e.g. a worktree).
    pub async fn move_session(&self, session_id: &str, directory: &Path) -> Result<()> {
        let path = format!("/api/session/{}/move", id(session_id));
        self.send(
            self.req(Method::POST, &path)
                .json(&json!({ "directory": directory })),
        )
        .await?;
        Ok(())
    }

    async fn get_in(&self, path: &str, directory: &Path) -> Result<Value> {
        self.send(
            self.req(Method::GET, path)
                .query(&[("location[directory]", directory)]),
        )
        .await
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

/// A worktree has uncommitted changes; removing it needs `force`.
#[derive(Debug)]
pub struct WorktreeDirty;

impl std::fmt::Display for WorktreeDirty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the worktree has uncommitted changes")
    }
}

impl std::error::Error for WorktreeDirty {}

fn array(v: &Value) -> Result<&Vec<Value>> {
    v.as_array()
        .ok_or_else(|| anyhow!("expected an array, got {}", v))
}

#[cfg(test)]
mod tests {
    use super::{Api, SseParser};
    use crate::api::model::PermissionDecision;

    type Exchange = (&'static str, &'static str, &'static str, &'static str);

    /// A one-connection-per-request HTTP server that checks each request
    /// line and body in order and answers with the scripted response.
    fn scripted_server(script: Vec<Exchange>) -> (Api, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (path, body, response, status) in script {
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
                let payload = String::from_utf8(payload).unwrap();
                if body.is_empty() {
                    assert_eq!(payload, body);
                } else {
                    // Compare as JSON: key order is an implementation detail.
                    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
                    assert_eq!(parse(&payload), parse(body), "{path}");
                }
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let api = Api {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: format!("http://{address}"),
            password: "test".into(),
        };
        (api, server)
    }

    /// Exercise the actual reqwest wire requests, including empty 204 replies.
    #[tokio::test]
    async fn permission_and_diff_http_contract() {
        let (api, server) = scripted_server(vec![
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
        ]);
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

    /// Models, agents, file search, switching and prompt attachments.
    #[tokio::test]
    async fn models_agents_and_mentions_http_contract() {
        use crate::api::model::{Mention, ModelRef, PromptInput};
        let (api, server) = scripted_server(vec![
            (
                "GET /api/model?location%5Bdirectory%5D=%2Frepo&types=chat ",
                "",
                r#"{"data":[{"id":"m1","providerID":"p","name":"M One","enabled":true,"variants":[{"id":"high"}]},{"id":"off","providerID":"p","enabled":false}]}"#,
                "200 OK",
            ),
            (
                "GET /api/model/default?location%5Bdirectory%5D=%2Frepo ",
                "",
                r#"{"data":{"id":"m1","providerID":"p","name":"M One"}}"#,
                "200 OK",
            ),
            (
                "GET /api/agent?location%5Bdirectory%5D=%2Frepo ",
                "",
                r#"{"data":[{"id":"build","name":"Build","mode":"primary","hidden":false},{"id":"general","name":"General","mode":"subagent","hidden":false},{"id":"title","name":"Title","mode":"primary","hidden":true}]}"#,
                "200 OK",
            ),
            (
                "GET /api/fs/find?location%5Bdirectory%5D=%2Frepo&query=ma&limit=20 ",
                "",
                r#"{"data":[{"path":"src/main.rs","type":"file"},{"path":"/","type":"directory"},{"path":"src/","type":"directory"}]}"#,
                "200 OK",
            ),
            (
                "POST /api/session/ses_a/model ",
                r#"{"model":{"id":"m1","providerID":"p","variant":"high"}}"#,
                "",
                "204 No Content",
            ),
            (
                "POST /api/session/ses_a/agent ",
                r#"{"agent":"plan"}"#,
                "",
                "204 No Content",
            ),
            (
                "POST /api/session/ses_a/prompt ",
                r#"{"agents":[{"mention":{"end":17,"start":9,"text":"@general"},"name":"general"}],"files":[{"mention":{"end":8,"start":0,"text":"@main.rs"},"name":"main.rs","uri":"file:///repo/main.rs"}],"text":"@main.rs @general"}"#,
                r#"{"data":{"id":"msg_1"}}"#,
                "200 OK",
            ),
        ]);
        let dir = std::path::Path::new("/repo");
        let models = api.models(dir).await.unwrap();
        assert_eq!(models.len(), 1, "disabled models are dropped");
        assert_eq!(models[0].variants, ["high"]);
        let default = api.default_model(dir).await.unwrap().unwrap();
        assert_eq!(
            (default.provider_id.as_str(), default.id.as_str()),
            ("p", "m1")
        );
        let agents = api.agents(dir).await.unwrap();
        assert!(agents[0].is_primary() && !agents[0].is_subagent());
        assert!(agents[1].is_subagent() && !agents[1].is_primary());
        assert!(!agents[2].is_primary(), "hidden agents aren't offered");
        let found = api.find_files(dir, "ma", 20).await.unwrap();
        assert_eq!(found.len(), 2, "the location root is dropped");
        assert!(!found[0].is_dir && found[1].is_dir);
        let model = ModelRef {
            provider_id: "p".into(),
            id: "m1".into(),
            variant: Some("high".into()),
        };
        api.switch_model("ses_a", &model).await.unwrap();
        api.switch_agent("ses_a", "plan").await.unwrap();
        let text = "@main.rs @general";
        let input = PromptInput {
            text: text.into(),
            files: vec![(
                "/repo/main.rs".into(),
                Mention::from_byte_range(text, 0..8).unwrap(),
            )],
            agents: vec![(
                "general".into(),
                Mention::from_byte_range(text, 9..17).unwrap(),
            )],
            ..Default::default()
        };
        api.prompt("ses_a", &input).await.unwrap();
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
