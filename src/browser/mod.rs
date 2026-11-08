//! ocdesk as opencode's desktop browser.
//!
//! opencode 2.2.6's `@opencode/plugin-browser` gives the agent browser tools
//! (`tools.browser.*` in code mode) and a `browser` skill, but only while a
//! desktop is attached to the session; the desktop owns the browser and runs
//! the operations. This module is that desktop (see
//! docs/browser-attach-experiment.md for the protocol as observed):
//!
//! - per opencode session: `POST /api/rpc/experimental.browser/attach`,
//!   held open; `rpc.experimental.browser.control` events (routed here from
//!   the event pump by `connectionID`) announce `attached`, `command` and
//!   `cancel`; commands are fetched with `command` and answered with `result`;
//!   the tab inventory is published with `state`.
//! - one headless Chromium (launched on the first tab, over a CDP pipe), one
//!   browser context per opencode session, one CDP target per tab.
//! - the Browser panel watches a tab through `Page.startScreencast`.
//!
//! Operations not implemented here answer `unsupported`, as opencode's own
//! desktop does for ones it lacks.

pub mod cdp;
pub mod chrome;
pub mod snapshot;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use crate::api::Api;
use cdp::{Cdp, CdpEvent};
use snapshot::RefTarget;

const NAV_TIMEOUT: Duration = Duration::from_secs(30);
const CONSOLE_KEEP: usize = 500;
const RPC: &str = "experimental.browser";

/// A failed operation as the plugin wants it: a short code and a message
/// that says what to do next.
#[derive(Debug)]
pub struct OpError {
    pub code: &'static str,
    pub message: String,
}

impl OpError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for OpError {
    fn from(e: anyhow::Error) -> Self {
        Self::new("failed", format!("{e:#}"))
    }
}

type OpResult = std::result::Result<(Value, Vec<Value>), OpError>;

#[derive(Clone, Debug, Default)]
pub struct Tab {
    pub id: String,
    target_id: String,
    /// CDP session (flattened) for this page.
    cdp_session: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub load_error: Option<String>,
    can_back: bool,
    can_forward: bool,
    generation: u64,
    refs: HashMap<String, RefTarget>,
    console: VecDeque<Value>,
}

impl Tab {
    fn json(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "url": clip(&self.url, 16_384),
            "title": clip(&self.title, 2_048),
            "loading": self.loading,
            "canGoBack": self.can_back,
            "canGoForward": self.can_forward,
            "generation": self.generation,
        });
        if let Some(e) = &self.load_error {
            v["loadError"] = json!(clip(e, 2_048));
        }
        v
    }
}

#[derive(Default)]
struct SessionBrowser {
    context_id: Option<String>,
    tabs: Vec<Tab>,
    focused: Option<String>,
}

struct Attachment {
    connection: String,
    /// True once the server confirmed (`attached`) and has our tab state.
    ready: watch::Sender<bool>,
    directory: PathBuf,
    api: Api,
    control: mpsc::UnboundedSender<Value>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, SessionBrowser>,
    attachments: HashMap<String, Attachment>,
    chrome_error: Option<String>,
}

/// What the Browser panel shows for a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub attached: bool,
    pub tabs: Vec<(String, String, String)>,
    pub focused: Option<String>,
    pub chrome_error: Option<String>,
}

/// One screencast frame (base64 JPEG) of a tab.
#[derive(Clone)]
pub struct ScreenFrame {
    pub jpeg_base64: String,
    /// The page viewport in CSS pixels.
    pub device_width: f32,
    pub device_height: f32,
}

#[derive(Clone)]
pub struct Host(Arc<Inner>);

struct Inner {
    rt: tokio::runtime::Handle,
    state: Mutex<State>,
    cdp: tokio::sync::Mutex<Option<Cdp>>,
    /// Bumped on every change the panel shows.
    changed: watch::Sender<u64>,
}

impl Host {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self(Arc::new(Inner {
            rt,
            state: Mutex::default(),
            cdp: tokio::sync::Mutex::new(None),
            changed: watch::channel(0).0,
        }))
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }

    fn bump(&self) {
        self.0.changed.send_modify(|v| *v += 1);
    }

    pub fn view(&self, session: &str) -> View {
        let st = self.0.state.lock().unwrap();
        let sb = st.sessions.get(session);
        View {
            attached: st.attachments.contains_key(session),
            tabs: sb
                .map(|s| {
                    s.tabs
                        .iter()
                        .map(|t| (t.id.clone(), t.title.clone(), t.url.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            focused: sb.and_then(|s| s.focused.clone()),
            chrome_error: st.chrome_error.clone(),
        }
    }

    // ---- attachments -----------------------------------------------------

    /// Attach as the session's desktop browser unless already attached. The
    /// agent's browser tools exist only while this holds.
    pub fn ensure_attached(
        &self,
        api: Api,
        session: String,
        directory: PathBuf,
    ) -> watch::Receiver<bool> {
        let (tx, rx) = mpsc::unbounded_channel();
        let connection = random_uuid();
        let (ready, ready_rx) = watch::channel(false);
        {
            let mut st = self.0.state.lock().unwrap();
            if let Some(a) = st.attachments.get(&session) {
                return a.ready.subscribe();
            }
            st.attachments.insert(
                session.clone(),
                Attachment {
                    ready,
                    connection: connection.clone(),
                    directory: directory.clone(),
                    api: api.clone(),
                    control: tx,
                },
            );
        }
        self.bump();
        let host = self.clone();
        self.0.rt.spawn(async move {
            let end = host
                .attachment(api, session.clone(), directory, connection.clone(), rx)
                .await;
            tracing::info!(%session, ?end, "browser attachment ended");
            let mut st = host.0.state.lock().unwrap();
            if st
                .attachments
                .get(&session)
                .is_some_and(|a| a.connection == connection)
            {
                st.attachments.remove(&session);
            }
            drop(st);
            host.bump();
        });
        ready_rx
    }

    /// Attach and wait (briefly) until the server confirms, so a new
    /// session's first turn already has the browser tools. Without this the
    /// first tool call can run before the attachment lands ("Unknown tool").
    pub async fn attach_and_wait(
        &self,
        api: Api,
        session: String,
        directory: PathBuf,
        timeout: Duration,
    ) {
        let mut ready = self.ensure_attached(api, session, directory);
        let _ = tokio::time::timeout(timeout, ready.wait_for(|r| *r)).await;
    }

    /// A `rpc.experimental.browser.control` event from the event stream.
    pub fn control(&self, data: Value) {
        let Some(conn) = data.get("connectionID").and_then(Value::as_str) else {
            return;
        };
        let st = self.0.state.lock().unwrap();
        if let Some(a) = st.attachments.values().find(|a| a.connection == conn) {
            let _ = a.control.send(data);
        }
    }

    async fn attachment(
        &self,
        api: Api,
        session: String,
        directory: PathBuf,
        connection: String,
        mut control: mpsc::UnboundedReceiver<Value>,
    ) -> Result<String> {
        let input = json!({ "sessionID": session, "connectionID": connection, "version": 4 });
        let attach = api.rpc_long(&directory, RPC, "attach", input);
        tokio::pin!(attach);
        loop {
            tokio::select! {
                end = &mut attach => return end.map(|v| v.as_str().unwrap_or("closed").to_string()),
                Some(c) = control.recv() => match c.get("type").and_then(Value::as_str) {
                    Some("attached") => {
                        self.publish(&session).await;
                        if let Some(a) = self.0.state.lock().unwrap().attachments.get(&session) {
                            let _ = a.ready.send(true);
                        }
                    }
                    Some("command") => {
                        let Some(req) = c.get("requestID").and_then(Value::as_str) else { continue };
                        let (host, req) = (self.clone(), req.to_string());
                        let (api, session, directory, connection) =
                            (api.clone(), session.clone(), directory.clone(), connection.clone());
                        tokio::spawn(async move {
                            if let Err(e) = host.run(&api, &directory, &session, &connection, &req).await {
                                tracing::warn!("browser command {req}: {e:#}");
                            }
                        });
                    }
                    // A cancelled request's result is ignored by the server.
                    _ => {}
                },
            }
        }
    }

    async fn run(
        &self,
        api: &Api,
        dir: &Path,
        session: &str,
        connection: &str,
        req: &str,
    ) -> Result<()> {
        let base = json!({ "sessionID": session, "connectionID": connection, "requestID": req });
        let command = api.rpc(dir, RPC, "command", base.clone()).await?;
        let action = command.get("action").cloned().unwrap_or(Value::Null);
        let ty = action
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let outcome = match self.exec(session, &ty, &action).await {
            Ok((value, files)) => {
                json!({ "type": "success", "result": { "value": value, "files": files } })
            }
            Err(e) => {
                json!({ "type": "failure", "code": e.code, "message": clip(&e.message, 2_000) })
            }
        };
        tracing::debug!(%ty, ok = outcome["type"] == "success", "browser command");
        let mut input = base;
        input["outcome"] = outcome;
        api.rpc(dir, RPC, "result", input).await?;
        Ok(())
    }

    /// Send the session's tab inventory; the server resolves tab ids only
    /// from this.
    async fn publish(&self, session: &str) {
        let (api, dir, input) = {
            let st = self.0.state.lock().unwrap();
            let Some(a) = st.attachments.get(session) else {
                return;
            };
            (
                a.api.clone(),
                a.directory.clone(),
                json!({
                    "sessionID": session,
                    "connectionID": a.connection,
                    "state": state_json(st.sessions.get(session)),
                }),
            )
        };
        if let Err(e) = api.rpc(&dir, RPC, "state", input).await {
            tracing::warn!("browser state: {e:#}");
        }
        self.bump();
    }

    // ---- the browser ------------------------------------------------------

    async fn cdp(&self) -> std::result::Result<Cdp, OpError> {
        let mut slot = self.0.cdp.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let Some(binary) = chrome::find() else {
            let msg = "No Chrome, Chromium or Edge found on this computer. Ask the user to install one (or set OCDESK_CHROME).";
            self.0.state.lock().unwrap().chrome_error = Some(msg.into());
            self.bump();
            return Err(OpError::new("unavailable", msg));
        };
        let chrome = chrome::Chrome::launch(&binary).await.map_err(|e| {
            self.0.state.lock().unwrap().chrome_error = Some(format!("{e:#}"));
            self.bump();
            OpError::from(e)
        })?;
        tracing::info!(binary = %binary.display(), "browser launched");
        let cdp = chrome.cdp.clone();
        cdp.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )
        .await?;
        {
            let mut st = self.0.state.lock().unwrap();
            st.chrome_error = None;
            // A previous browser's tabs are gone.
            for sb in st.sessions.values_mut() {
                *sb = SessionBrowser::default();
            }
        }
        self.watch_events(cdp.subscribe());
        let mut child = chrome.child;
        let host = self.clone();
        self.0.rt.spawn(async move {
            let _ = child.wait().await;
            tracing::info!("browser exited");
            host.browser_gone().await;
        });
        *slot = Some(cdp.clone());
        Ok(cdp)
    }

    /// The browser died: its tabs are gone. Republish so the agent's stale
    /// tab ids fail with "unknown tab" rather than "browser closed".
    async fn browser_gone(&self) {
        let sessions: Vec<String> = {
            let mut st = self.0.state.lock().unwrap();
            for sb in st.sessions.values_mut() {
                *sb = SessionBrowser::default();
            }
            st.attachments.keys().cloned().collect()
        };
        for s in sessions {
            self.publish(&s).await;
        }
    }

    /// Keep tab metadata and console logs current from CDP events.
    fn watch_events(&self, mut rx: broadcast::Receiver<CdpEvent>) {
        let host = self.clone();
        self.0.rt.spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if ev.method == "ocdesk.closed" {
                    return;
                }
                if let Some(owner) = host.on_event(&ev) {
                    host.publish(&owner).await;
                }
            }
        });
    }

    /// Apply one event; returns the session whose `state` changed.
    fn on_event(&self, ev: &CdpEvent) -> Option<String> {
        let mut st = self.0.state.lock().unwrap();
        match ev.method.as_str() {
            "Target.targetInfoChanged" => {
                let info = &ev.params["targetInfo"];
                let target = info["targetId"].as_str()?;
                let (owner, tab) = find_tab(&mut st, |t| t.target_id == target)?;
                let (url, title) = (str_of(info, "url"), str_of(info, "title"));
                if tab.url == url && tab.title == title {
                    return None;
                }
                tab.url = url;
                tab.title = title;
                Some(owner)
            }
            "Page.frameStartedLoading" | "Page.loadEventFired" => {
                let s = ev.session.as_deref()?;
                let (owner, tab) = find_tab(&mut st, |t| t.cdp_session == s)?;
                tab.loading = ev.method == "Page.frameStartedLoading";
                Some(owner)
            }
            "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" => {
                let s = ev.session.as_deref()?;
                let (_, tab) = find_tab(&mut st, |t| t.cdp_session == s)?;
                let entry = console_entry(&ev.method, &ev.params, tab.console.len());
                if tab.console.len() == CONSOLE_KEEP {
                    tab.console.pop_front();
                }
                tab.console.push_back(entry);
                None
            }
            _ => None,
        }
    }

    fn tab(&self, session: &str, tab_id: &str) -> std::result::Result<Tab, OpError> {
        let st = self.0.state.lock().unwrap();
        st.sessions
            .get(session)
            .and_then(|s| s.tabs.iter().find(|t| t.id == tab_id))
            .cloned()
            .ok_or_else(|| {
                OpError::new(
                    "not_found",
                    format!("Unknown tab {tab_id}. Call browser.tabs.list for current tab IDs."),
                )
            })
    }

    fn update_tab(&self, session: &str, tab_id: &str, f: impl FnOnce(&mut Tab)) -> Option<Tab> {
        let mut st = self.0.state.lock().unwrap();
        let t = st
            .sessions
            .get_mut(session)?
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)?;
        f(t);
        Some(t.clone())
    }

    // ---- operations ---------------------------------------------------------

    async fn exec(&self, session: &str, ty: &str, a: &Value) -> OpResult {
        let tab_id = a.get("tabID").and_then(Value::as_str).unwrap_or("");
        match ty {
            "tabs.list" => {
                let st = self.0.state.lock().unwrap();
                Ok((state_json(st.sessions.get(session)), vec![]))
            }
            "tabs.open" => self.open_tab(session, a).await,
            "tabs.focus" => {
                let t = self.tab(session, tab_id)?;
                self.0
                    .state
                    .lock()
                    .unwrap()
                    .sessions
                    .entry(session.into())
                    .or_default()
                    .focused = Some(t.id.clone());
                self.publish(session).await;
                Ok((t.json(), vec![]))
            }
            "tabs.close" => {
                let t = self.tab(session, tab_id)?;
                let cdp = self.cdp().await?;
                let _ = cdp
                    .call(
                        "Target.closeTarget",
                        json!({ "targetId": t.target_id }),
                        None,
                    )
                    .await;
                {
                    let mut st = self.0.state.lock().unwrap();
                    let sb = st.sessions.entry(session.into()).or_default();
                    sb.tabs.retain(|x| x.id != t.id);
                    if sb.focused.as_deref() == Some(&t.id) {
                        sb.focused = sb.tabs.last().map(|x| x.id.clone());
                    }
                }
                self.publish(session).await;
                let st = self.0.state.lock().unwrap();
                Ok((state_json(st.sessions.get(session)), vec![]))
            }
            "navigate" => {
                let url = a
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("about:blank");
                let t = self.tab(session, tab_id)?;
                Ok((
                    self.navigate(session, &t, NavTo::Url(url)).await?.json(),
                    vec![],
                ))
            }
            "back" | "forward" | "reload" => {
                let t = self.tab(session, tab_id)?;
                let to = match ty {
                    "back" => NavTo::History(-1),
                    "forward" => NavTo::History(1),
                    _ => NavTo::Reload,
                };
                Ok((self.navigate(session, &t, to).await?.json(), vec![]))
            }
            "snapshot" => {
                let t = self.tab(session, tab_id)?;
                let cdp = self.cdp().await?;
                let s = Some(t.cdp_session.as_str());
                // Box-model lookups by backend node id want a document.
                cdp.call("DOM.getDocument", json!({ "depth": 0 }), s)
                    .await?;
                let tree = cdp
                    .call("Accessibility.getFullAXTree", json!({}), s)
                    .await?;
                let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
                let depth = a.get("depth").and_then(Value::as_u64).map(|d| d as usize);
                let snap = snapshot::render(&nodes, depth);
                let t = self
                    .update_tab(session, tab_id, |t| t.refs = snap.refs)
                    .unwrap_or(t);
                Ok((
                    json!({ "tab": t.json(), "content": snap.content, "truncated": snap.truncated }),
                    vec![],
                ))
            }
            "click" | "hover" => {
                let t = self.tab(session, tab_id)?;
                let target = self.target(&t, a)?;
                if ty == "click" && target.disabled {
                    return Err(OpError::new(
                        "failed",
                        format!(
                            "{} \"{}\" is disabled; it can't be clicked.",
                            target.role, target.name
                        ),
                    ));
                }
                let cdp = self.cdp().await?;
                let (x, y) = center(&cdp, &t, target.backend_node_id).await?;
                let button = a.get("button").and_then(Value::as_str).unwrap_or("left");
                let count = a.get("clickCount").and_then(Value::as_u64).unwrap_or(1);
                let mods = modifiers(a.get("modifiers"));
                let s = Some(t.cdp_session.as_str());
                cdp.call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": mods }),
                    s,
                )
                .await?;
                if ty == "click" {
                    for ev in ["mousePressed", "mouseReleased"] {
                        cdp.call("Input.dispatchMouseEvent", json!({ "type": ev, "x": x, "y": y, "button": button, "clickCount": count, "modifiers": mods }), s).await?;
                    }
                }
                Ok((self.refreshed(session, &t).await.json(), vec![]))
            }
            "fill" => {
                let t = self.tab(session, tab_id)?;
                let target = self.target(&t, a)?;
                let text = a.get("text").and_then(Value::as_str).unwrap_or("");
                let cdp = self.cdp().await?;
                let s = Some(t.cdp_session.as_str());
                let obj = cdp
                    .call(
                        "DOM.resolveNode",
                        json!({ "backendNodeId": target.backend_node_id }),
                        s,
                    )
                    .await?;
                let object_id = obj["object"]["objectId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("element is gone; take a new snapshot"))?;
                // Focus and select the current text, so typing replaces it.
                cdp.call("Runtime.callFunctionOn", json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(){this.focus();if(typeof this.select==='function'){this.select();}else{const r=document.createRange();r.selectNodeContents(this);const s=getSelection();s.removeAllRanges();s.addRange(r);}}",
                }), s).await?;
                if text.is_empty() {
                    for ev in ["keyDown", "keyUp"] {
                        cdp.call("Input.dispatchKeyEvent", json!({ "type": ev, "key": "Delete", "code": "Delete", "windowsVirtualKeyCode": 46 }), s).await?;
                    }
                } else {
                    cdp.call("Input.insertText", json!({ "text": text }), s)
                        .await?;
                }
                Ok((self.refreshed(session, &t).await.json(), vec![]))
            }
            "press" => {
                let t = self.tab(session, tab_id)?;
                let key = a.get("key").and_then(Value::as_str).unwrap_or("");
                let k = key_event(key).ok_or_else(|| OpError::new("invalid", format!("Unknown key {key:?}. Use names like Enter, Tab, ArrowDown or Control+a.")))?;
                let cdp = self.cdp().await?;
                let s = Some(t.cdp_session.as_str());
                let mut down = json!({ "type": "keyDown", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk, "modifiers": k.modifiers });
                if let Some(text) = &k.text {
                    down["text"] = json!(text);
                }
                cdp.call("Input.dispatchKeyEvent", down, s).await?;
                cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk, "modifiers": k.modifiers }), s).await?;
                Ok((self.refreshed(session, &t).await.json(), vec![]))
            }
            "scroll" => {
                let t = self.tab(session, tab_id)?;
                let cdp = self.cdp().await?;
                let (dx, dy) = (num(a, "deltaX"), num(a, "deltaY"));
                let (w, h) = chrome::VIEWPORT;
                cdp.call("Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": w / 2, "y": h / 2, "deltaX": dx, "deltaY": dy }), Some(&t.cdp_session)).await?;
                Ok((t.json(), vec![]))
            }
            "wait" => {
                let t = self.tab(session, tab_id)?;
                let timeout = Duration::from_millis(
                    a.get("timeoutMs").and_then(Value::as_u64).unwrap_or(10_000),
                );
                let cond = a.get("condition").and_then(Value::as_str).unwrap_or("load");
                let text = a.get("text").and_then(Value::as_str).unwrap_or("");
                self.wait(&t, cond, text, timeout).await?;
                Ok((self.refreshed(session, &t).await.json(), vec![]))
            }
            "screenshot" => self.screenshot(session, tab_id, a).await,
            "console" => {
                let t = self.tab(session, tab_id)?;
                let level = a.get("level").and_then(Value::as_str);
                let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
                let all: Vec<&Value> = t
                    .console
                    .iter()
                    .filter(|m| level.is_none_or(|l| m["level"] == l))
                    .collect();
                let start = all.len().saturating_sub(limit);
                Ok((
                    json!({
                        "tab": t.json(),
                        "messages": all[start..],
                        "truncated": start > 0,
                        "dropped": 0,
                    }),
                    vec![],
                ))
            }
            other => Err(OpError::new(
                "unsupported",
                format!(
                    "ocdesk's browser doesn't support {other} yet. Use snapshot, click, fill, press, scroll, wait, screenshot or console."
                ),
            )),
        }
    }

    async fn open_tab(&self, session: &str, a: &Value) -> OpResult {
        let cdp = self.cdp().await?;
        let context = {
            let existing = self
                .0
                .state
                .lock()
                .unwrap()
                .sessions
                .get(session)
                .and_then(|s| s.context_id.clone());
            match existing {
                Some(c) => c,
                None => {
                    let r = cdp
                        .call("Target.createBrowserContext", json!({}), None)
                        .await?;
                    let c = str_of(&r, "browserContextId");
                    self.0
                        .state
                        .lock()
                        .unwrap()
                        .sessions
                        .entry(session.into())
                        .or_default()
                        .context_id = Some(c.clone());
                    c
                }
            }
        };
        let r = cdp
            .call(
                "Target.createTarget",
                json!({ "url": "about:blank", "browserContextId": context }),
                None,
            )
            .await?;
        let target_id = str_of(&r, "targetId");
        let r = cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let cdp_session = str_of(&r, "sessionId");
        let s = Some(cdp_session.as_str());
        for domain in ["Page.enable", "Runtime.enable"] {
            cdp.call(domain, json!({}), s).await?;
        }
        let (w, h) = chrome::VIEWPORT;
        cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false }),
            s,
        )
        .await?;
        let tab = Tab {
            id: format!("tab_{}", random_uuid()),
            target_id,
            cdp_session,
            url: "about:blank".into(),
            ..Default::default()
        };
        {
            let mut st = self.0.state.lock().unwrap();
            let sb = st.sessions.entry(session.into()).or_default();
            sb.tabs.push(tab.clone());
            if a.get("focus").and_then(Value::as_bool) != Some(false) || sb.focused.is_none() {
                sb.focused = Some(tab.id.clone());
            }
        }
        let url = a
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty() && *u != "about:blank");
        let tab = match url {
            Some(u) => self.navigate(session, &tab, NavTo::Url(u)).await?,
            None => tab,
        };
        // Inventory before the result: the server looks tab ids up there.
        self.publish(session).await;
        Ok((tab.json(), vec![]))
    }

    async fn navigate(
        &self,
        session: &str,
        t: &Tab,
        to: NavTo<'_>,
    ) -> std::result::Result<Tab, OpError> {
        let cdp = self.cdp().await?;
        let s = Some(t.cdp_session.as_str());
        let mut events = cdp.subscribe();
        let mut error = None;
        match to {
            NavTo::Url(url) => {
                let r = cdp.call("Page.navigate", json!({ "url": url }), s).await?;
                error = r
                    .get("errorText")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            NavTo::Reload => {
                cdp.call("Page.reload", json!({}), s).await?;
            }
            NavTo::History(delta) => {
                let h = cdp.call("Page.getNavigationHistory", json!({}), s).await?;
                let i = h["currentIndex"].as_i64().unwrap_or(0) + delta;
                let entry = h["entries"]
                    .as_array()
                    .and_then(|e| e.get(i as usize))
                    .filter(|_| i >= 0);
                let Some(entry) = entry else {
                    return Err(OpError::new(
                        "failed",
                        if delta < 0 {
                            "There is no page to go back to."
                        } else {
                            "There is no page to go forward to."
                        },
                    ));
                };
                cdp.call(
                    "Page.navigateToHistoryEntry",
                    json!({ "entryId": entry["id"] }),
                    s,
                )
                .await?;
            }
        }
        if error.is_none() {
            let wait = async {
                while let Ok(ev) = events.recv().await {
                    if ev.method == "Page.loadEventFired" && ev.session.as_deref() == s {
                        return;
                    }
                }
            };
            let _ = tokio::time::timeout(NAV_TIMEOUT, wait).await;
        }
        let t = self.refreshed(session, t).await;
        let t = self
            .update_tab(session, &t.id, |t| {
                t.generation += 1;
                t.refs.clear();
                t.console.clear();
                t.loading = false;
                t.load_error = error.clone();
            })
            .unwrap_or(t);
        self.publish(session).await;
        Ok(t)
    }

    /// Re-read url, title and history flags.
    async fn refreshed(&self, session: &str, t: &Tab) -> Tab {
        let Ok(cdp) = self.cdp().await else {
            return t.clone();
        };
        let info = cdp
            .call(
                "Target.getTargetInfo",
                json!({ "targetId": t.target_id }),
                None,
            )
            .await
            .ok();
        let hist = cdp
            .call("Page.getNavigationHistory", json!({}), Some(&t.cdp_session))
            .await
            .ok();
        self.update_tab(session, &t.id, |t| {
            if let Some(i) = &info {
                t.url = str_of(&i["targetInfo"], "url");
                t.title = str_of(&i["targetInfo"], "title");
            }
            if let Some(h) = &hist {
                let i = h["currentIndex"].as_i64().unwrap_or(0);
                let n = h["entries"].as_array().map_or(0, Vec::len) as i64;
                t.can_back = i > 0;
                t.can_forward = i + 1 < n;
            }
        })
        .unwrap_or_else(|| t.clone())
    }

    fn target(&self, t: &Tab, a: &Value) -> std::result::Result<RefTarget, OpError> {
        let r = a
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches('@');
        t.refs.get(r).cloned().ok_or_else(|| {
            OpError::new("stale_ref", format!("Ref {r} isn't in this tab's latest snapshot. Take a new browser.snapshot and use its refs."))
        })
    }

    async fn wait(
        &self,
        t: &Tab,
        cond: &str,
        text: &str,
        timeout: Duration,
    ) -> std::result::Result<(), OpError> {
        let cdp = self.cdp().await?;
        let s = Some(t.cdp_session.as_str());
        let expr = match cond {
            "load" => "document.readyState === 'complete'".to_string(),
            "text" => format!(
                "!!document.body && document.body.innerText.includes({})",
                json!(text)
            ),
            "textGone" => format!(
                "!document.body || !document.body.innerText.includes({})",
                json!(text)
            ),
            other => {
                return Err(OpError::new(
                    "invalid",
                    format!("Unknown wait condition {other}."),
                ));
            }
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let r = cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": expr, "returnByValue": true }),
                    s,
                )
                .await?;
            if r["result"]["value"] == json!(true) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(OpError::new(
                    "timeout",
                    format!(
                        "Waited {} ms for {cond}{}. Take a snapshot to see the page.",
                        timeout.as_millis(),
                        if text.is_empty() {
                            String::new()
                        } else {
                            format!(" {text:?}")
                        }
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn screenshot(&self, session: &str, tab_id: &str, a: &Value) -> OpResult {
        let t = self.tab(session, tab_id)?;
        let cdp = self.cdp().await?;
        let s = Some(t.cdp_session.as_str());
        let format = match a.get("format").and_then(Value::as_str) {
            Some("jpeg") => "jpeg",
            Some("webp") => "webp",
            _ => "png",
        };
        let mut params = json!({ "format": format });
        if let Some(q) = a
            .get("quality")
            .and_then(Value::as_u64)
            .filter(|_| format != "png")
        {
            params["quality"] = json!(q);
        }
        if a.get("ref").is_some() {
            let target = self.target(&t, a)?;
            cdp.call(
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": target.backend_node_id }),
                s,
            )
            .await?;
            let quads = cdp
                .call(
                    "DOM.getContentQuads",
                    json!({ "backendNodeId": target.backend_node_id }),
                    s,
                )
                .await?;
            let q: Vec<f64> = quads["quads"][0]
                .as_array()
                .map(|q| q.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            if q.len() == 8 {
                let xs = [q[0], q[2], q[4], q[6]];
                let ys = [q[1], q[3], q[5], q[7]];
                let (x0, y0) = (
                    xs.iter().cloned().fold(f64::MAX, f64::min),
                    ys.iter().cloned().fold(f64::MAX, f64::min),
                );
                let (x1, y1) = (
                    xs.iter().cloned().fold(f64::MIN, f64::max),
                    ys.iter().cloned().fold(f64::MIN, f64::max),
                );
                params["clip"] =
                    json!({ "x": x0, "y": y0, "width": x1 - x0, "height": y1 - y0, "scale": 1 });
            }
        } else if a.get("fullPage").and_then(Value::as_bool) == Some(true) {
            let m = cdp.call("Page.getLayoutMetrics", json!({}), s).await?;
            let size = &m["cssContentSize"];
            params["captureBeyondViewport"] = json!(true);
            params["clip"] = json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": 1 });
        }
        let r = cdp.call("Page.captureScreenshot", params, s).await?;
        let data = r["data"].as_str().unwrap_or_default().to_string();
        let file = json!({
            "id": format!("file_{}", random_uuid()),
            "name": format!("screenshot.{format}"),
            "mime": format!("image/{format}"),
            "data": data,
        });
        Ok((json!({ "tab": t.json() }), vec![file]))
    }

    // ---- the panel ----------------------------------------------------------

    /// Stream a tab's screen. Frames stop when the receiver is dropped.
    pub fn screencast(
        &self,
        session: &str,
        tab_id: &str,
        max_width: u32,
    ) -> Option<mpsc::Receiver<ScreenFrame>> {
        let t = self.tab(session, tab_id).ok()?;
        let cdp = self.0.cdp.try_lock().ok()?.clone()?;
        let (tx, rx) = mpsc::channel(2);
        self.0.rt.spawn(async move {
            let s = Some(t.cdp_session.as_str());
            let mut events = cdp.subscribe();
            let params = json!({ "format": "jpeg", "quality": 70, "maxWidth": max_width, "everyNthFrame": 1 });
            if cdp.call("Page.startScreencast", params, s).await.is_err() {
                return;
            }
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    ev = events.recv() => match ev {
                        Ok(ev) if ev.method == "Page.screencastFrame" && ev.session.as_deref() == s => {
                            let p = &ev.params;
                            // Ack first: Chrome sends the next frame only after it.
                            let _ = cdp.call("Page.screencastFrameAck", json!({ "sessionId": p["sessionId"] }), s).await;
                            let frame = ScreenFrame {
                                jpeg_base64: p["data"].as_str().unwrap_or_default().to_string(),
                                device_width: p["metadata"]["deviceWidth"].as_f64().unwrap_or(0.) as f32,
                                device_height: p["metadata"]["deviceHeight"].as_f64().unwrap_or(0.) as f32,
                            };
                            // Drop frames the panel hasn't taken yet.
                            let _ = tx.try_send(frame);
                        }
                        Ok(ev) if ev.method == "ocdesk.closed" => break,
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    },
                }
            }
            let _ = cdp.call("Page.stopScreencast", json!({}), s).await;
        });
        Some(rx)
    }

    /// Forward the user's input (Take control) to a tab: a CDP `Input.*` call.
    pub fn input(&self, session: &str, tab_id: &str, method: &'static str, params: Value) {
        let Ok(t) = self.tab(session, tab_id) else {
            return;
        };
        let Some(cdp) = self.0.cdp.try_lock().ok().and_then(|c| c.clone()) else {
            return;
        };
        self.0.rt.spawn(async move {
            let _ = cdp.call(method, params, Some(&t.cdp_session)).await;
        });
    }

    /// Show a tab in the panel (and tell the server which tab is in front).
    pub fn focus(&self, session: &str, tab_id: &str) {
        {
            let mut st = self.0.state.lock().unwrap();
            let Some(sb) = st.sessions.get_mut(session) else {
                return;
            };
            if !sb.tabs.iter().any(|t| t.id == tab_id) {
                return;
            }
            sb.focused = Some(tab_id.to_string());
        }
        let (host, session) = (self.clone(), session.to_string());
        self.0.rt.spawn(async move { host.publish(&session).await });
    }
}

enum NavTo<'a> {
    Url(&'a str),
    Reload,
    History(i64),
}

fn state_json(sb: Option<&SessionBrowser>) -> Value {
    json!({
        "tabs": sb.map(|s| s.tabs.iter().map(Tab::json).collect::<Vec<_>>()).unwrap_or_default(),
        "focusedTabID": sb.and_then(|s| s.focused.clone()),
    })
}

fn find_tab(st: &mut State, pred: impl Fn(&Tab) -> bool) -> Option<(String, &mut Tab)> {
    st.sessions.iter_mut().find_map(|(sid, sb)| {
        sb.tabs
            .iter_mut()
            .find(|t| pred(t))
            .map(|t| (sid.clone(), t))
    })
}

async fn center(
    cdp: &Cdp,
    t: &Tab,
    backend_node_id: i64,
) -> std::result::Result<(f64, f64), OpError> {
    let s = Some(t.cdp_session.as_str());
    cdp.call(
        "DOM.scrollIntoViewIfNeeded",
        json!({ "backendNodeId": backend_node_id }),
        s,
    )
    .await
    .map_err(|e| {
        OpError::new(
            "stale_ref",
            format!("The element is gone ({e:#}). Take a new snapshot."),
        )
    })?;
    let quads = cdp
        .call(
            "DOM.getContentQuads",
            json!({ "backendNodeId": backend_node_id }),
            s,
        )
        .await?;
    let q: Vec<f64> = quads["quads"][0]
        .as_array()
        .map(|q| q.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    if q.len() != 8 {
        return Err(OpError::new(
            "failed",
            "The element isn't visible (it has no box). Scroll or take a new snapshot.",
        ));
    }
    Ok((
        (q[0] + q[2] + q[4] + q[6]) / 4.,
        (q[1] + q[3] + q[5] + q[7]) / 4.,
    ))
}

fn console_entry(method: &str, p: &Value, n: usize) -> Value {
    let (level, text) = if method == "Runtime.exceptionThrown" {
        let d = &p["exceptionDetails"];
        let text = d["exception"]["description"]
            .as_str()
            .or(d["text"].as_str())
            .unwrap_or("Uncaught error");
        ("error", text.to_string())
    } else {
        let level = match p["type"].as_str().unwrap_or("log") {
            "warning" => "warning",
            "error" | "assert" => "error",
            "debug" | "trace" => "debug",
            _ => "info",
        };
        let text = p["args"]
            .as_array()
            .map(|args| {
                args.iter()
                    .map(|a| match a.get("value") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => a["description"].as_str().unwrap_or("").to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        (level, text)
    };
    let truncated = text.len() > snapshot::MAX_TEXT;
    json!({
        "id": format!("c{}", n + 1),
        "timestampMs": p["timestamp"].as_f64().unwrap_or(0.),
        "level": level,
        "text": clip(&text, snapshot::MAX_TEXT),
        "textTruncated": truncated,
    })
}

struct KeyEvent {
    key: String,
    code: String,
    vk: u32,
    text: Option<String>,
    modifiers: u32,
}

/// `Enter`, `ArrowDown`, `a`, `Control+a`, `Shift+Tab`, …
fn key_event(spec: &str) -> Option<KeyEvent> {
    let parts: Vec<&str> = spec.split('+').collect();
    let (key, mods) = parts.split_last()?;
    let mut modifiers = 0;
    for m in mods {
        modifiers |= match *m {
            "Alt" => 1,
            "Control" | "Ctrl" => 2,
            "Meta" | "Cmd" | "Command" => 4,
            "Shift" => 8,
            _ => return None,
        };
    }
    let named: &[(&str, u32, Option<&str>)] = &[
        ("Enter", 13, Some("\r")),
        ("Tab", 9, None),
        ("Escape", 27, None),
        ("Backspace", 8, None),
        ("Delete", 46, None),
        ("Space", 32, Some(" ")),
        ("ArrowLeft", 37, None),
        ("ArrowUp", 38, None),
        ("ArrowRight", 39, None),
        ("ArrowDown", 40, None),
        ("Home", 36, None),
        ("End", 35, None),
        ("PageUp", 33, None),
        ("PageDown", 34, None),
    ];
    if let Some((name, vk, text)) = named.iter().find(|(n, ..)| n.eq_ignore_ascii_case(key)) {
        let key = if *name == "Space" { " " } else { name };
        return Some(KeyEvent {
            key: key.into(),
            code: name.to_string(),
            vk: *vk,
            text: text.map(str::to_string),
            modifiers,
        });
    }
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let upper = c.to_ascii_uppercase();
    let code = if c.is_ascii_alphabetic() {
        format!("Key{upper}")
    } else if c.is_ascii_digit() {
        format!("Digit{c}")
    } else {
        String::new()
    };
    Some(KeyEvent {
        key: c.to_string(),
        code,
        vk: if c.is_ascii_alphanumeric() {
            upper as u32
        } else {
            0
        },
        // With Control/Meta held it's a shortcut, not typed text.
        text: (modifiers & 0b110 == 0).then(|| c.to_string()),
        modifiers,
    })
}

fn modifiers(v: Option<&Value>) -> u32 {
    v.and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(Value::as_str)
                .map(|m| match m {
                    "Alt" => 1,
                    "Control" => 2,
                    "Meta" => 4,
                    "Shift" => 8,
                    _ => 0,
                })
                .fold(0, |a, b| a | b)
        })
        .unwrap_or(0)
}

fn num(a: &Value, k: &str) -> f64 {
    a.get(k).and_then(Value::as_f64).unwrap_or(0.)
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// A random RFC 4122 v4 uuid (the protocol's tab/file ids embed one).
fn random_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_protocol() {
        let id = format!("tab_{}", random_uuid());
        assert_eq!(id.len(), 40);
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn keys() {
        let k = key_event("Enter").unwrap();
        assert_eq!(
            (k.key.as_str(), k.vk, k.text.as_deref()),
            ("Enter", 13, Some("\r"))
        );
        let k = key_event("Control+a").unwrap();
        assert_eq!((k.code.as_str(), k.modifiers, k.text), ("KeyA", 2, None));
        let k = key_event("Shift+Tab").unwrap();
        assert_eq!((k.key.as_str(), k.modifiers), ("Tab", 8));
        assert!(key_event("Hyper+x").is_none());
        assert!(key_event("NotAKey").is_none());
        assert_eq!(key_event("arrowdown").unwrap().key, "ArrowDown");
    }

    #[test]
    fn console_entries() {
        let e = console_entry(
            "Runtime.consoleAPICalled",
            &json!({"type": "warning", "args": [{"type": "string", "value": "low disk"}, {"type": "number", "value": 3}], "timestamp": 5.0}),
            0,
        );
        assert_eq!(
            (e["level"].as_str(), e["text"].as_str(), e["id"].as_str()),
            (Some("warning"), Some("low disk 3"), Some("c1"))
        );
        let e = console_entry(
            "Runtime.exceptionThrown",
            &json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x is undefined"}}}),
            4,
        );
        assert_eq!(
            (e["level"].as_str(), e["text"].as_str()),
            (Some("error"), Some("TypeError: x is undefined"))
        );
    }

    #[test]
    fn tab_json_shape() {
        let t = Tab {
            id: "tab_x".into(),
            url: "https://a.dev/".into(),
            title: "A".into(),
            load_error: Some("net::ERR".into()),
            ..Default::default()
        };
        let v = t.json();
        for k in [
            "id",
            "url",
            "title",
            "loading",
            "canGoBack",
            "canGoForward",
            "generation",
            "loadError",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(state_json(None), json!({"tabs": [], "focusedTabID": null}));
    }
}
