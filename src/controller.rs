//! Turns user intents into API calls and API results into store updates.
//!
//! Owns the sidecar lifecycle (start, crash detection, auto-restart) and the
//! SSE pump. All network work runs on the Tokio runtime; results come back to
//! the GPUI thread through channels awaited in `cx.spawn` tasks.

use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui_kit::{Context, Entity, Task};

use crate::api::events::RunState;
use crate::api::{Api, Event};
use crate::sidecar::{self, SidecarConfig};
use crate::store::{Connection, Store};

/// Give up auto-restarting after this many crashes within [`CRASH_WINDOW`].
const MAX_QUICK_RESTARTS: u32 = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

pub struct Controller {
    rt: tokio::runtime::Handle,
    store: Entity<Store>,
    working_dir: PathBuf,
    api: Option<Api>,
    /// Bumped on every sidecar (re)start so stale callbacks can be ignored.
    generation: u64,
    pump: Option<(tokio::task::AbortHandle, Task<()>)>,
    exit_watch: Option<Task<()>>,
    recent_crashes: Vec<Instant>,
}

impl Controller {
    pub fn new(
        rt: tokio::runtime::Handle,
        store: Entity<Store>,
        working_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            rt,
            store,
            working_dir,
            api: None,
            generation: 0,
            pump: None,
            exit_watch: None,
            recent_crashes: Vec::new(),
        };
        this.start_sidecar(cx);
        this
    }

    pub fn working_dir(&self) -> &PathBuf {
        &self.working_dir
    }

    /// Run `fut` on the Tokio runtime and hand the result back on the UI thread.
    fn run<T, F>(
        &self,
        cx: &mut Context<Self>,
        fut: F,
        done: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |this, cx| done(this, res, cx));
            }
        })
        .detach();
    }

    // ── sidecar lifecycle ────────────────────────────────────────────────

    /// Kill whatever is running and start a fresh sidecar.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.recent_crashes.clear();
        sidecar::kill_sidecar();
        self.start_sidecar(cx);
    }

    fn start_sidecar(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.api = None;
        self.stop_pump();
        self.exit_watch = None;
        self.store
            .update(cx, |s, cx| s.set_connection(Connection::Starting, cx));

        let working_dir = self.working_dir.clone();
        self.run(
            cx,
            async move {
                let binary = sidecar::find_binary().ok_or_else(|| {
                    anyhow!(
                        "opencode is not installed or not on PATH.\n\
                         Install it with: curl -fsSL https://opencode.ai/install | bash"
                    )
                })?;
                sidecar::spawn(SidecarConfig {
                    binary,
                    working_dir,
                })
                .await
            },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                let (sc, exit_rx) = match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("sidecar failed to start: {e:#}");
                        this.store.update(cx, |s, cx| {
                            s.set_connection(
                                Connection::SidecarDown {
                                    message: format!("{e:#}"),
                                    last_lines: vec![],
                                },
                                cx,
                            )
                        });
                        return;
                    }
                };
                let api = match Api::new(&sc) {
                    Ok(api) => api,
                    Err(e) => {
                        this.store.update(cx, |s, cx| {
                            s.set_connection(
                                Connection::SidecarDown {
                                    message: format!("{e:#}"),
                                    last_lines: vec![],
                                },
                                cx,
                            )
                        });
                        return;
                    }
                };
                this.store.update(cx, |s, cx| {
                    s.opencode_version = Some(sc.version.clone());
                    s.set_connection(Connection::Connecting, cx);
                });
                this.api = Some(api.clone());
                this.watch_exit(generation, exit_rx, cx);
                this.start_pump(api, cx);
            },
        );
    }

    fn watch_exit(
        &mut self,
        generation: u64,
        exit_rx: tokio::sync::oneshot::Receiver<sidecar::SidecarExit>,
        cx: &mut Context<Self>,
    ) {
        self.exit_watch = Some(cx.spawn(async move |this, cx| {
            let Ok(exit) = exit_rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.api = None;
                this.stop_pump();
                let now = Instant::now();
                this.recent_crashes
                    .retain(|t| now.duration_since(*t) < CRASH_WINDOW);
                this.recent_crashes.push(now);
                let message = format!("opencode stopped unexpectedly (exit code {:?})", exit.code);
                this.store.update(cx, |s, cx| {
                    s.set_connection(
                        Connection::SidecarDown {
                            message,
                            last_lines: exit.last_lines,
                        },
                        cx,
                    );
                    // Nothing is running any more.
                    let busy: Vec<String> = s.status.keys().cloned().collect();
                    for id in busy {
                        s.set_status(&id, RunState::Idle, cx);
                    }
                });
                if this.recent_crashes.len() as u32 <= MAX_QUICK_RESTARTS {
                    tracing::warn!("restarting sidecar automatically");
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                        let _ = this.update(cx, |this, cx| {
                            if this.generation == generation {
                                this.start_sidecar(cx);
                            }
                        });
                    })
                    .detach();
                }
            });
        }));
    }

    fn start_pump(&mut self, api: Api, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::unbounded();
        let handle = self
            .rt
            .spawn(async move { api.run_events(tx).await })
            .abort_handle();
        let task = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |this, cx| this.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        });
        self.stop_pump();
        self.pump = Some((handle, task));
    }

    /// Dropping a tokio `AbortHandle` does not stop the task; abort explicitly.
    fn stop_pump(&mut self) {
        if let Some((handle, _task)) = self.pump.take() {
            handle.abort();
        }
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::Connected => {
                #[cfg(debug_assertions)]
                if std::env::var_os("OCDESK_DEBUG_PANIC").is_some() {
                    panic!("OCDESK_DEBUG_PANIC: deliberate panic to test sidecar teardown");
                }
                self.store
                    .update(cx, |s, cx| s.set_connection(Connection::Connected, cx));
                self.resync(cx);
            }
            Event::Disconnected => {
                self.store.update(cx, |s, cx| {
                    if s.connection() == Connection::Connected {
                        s.set_connection(Connection::Reconnecting, cx);
                    }
                });
            }
            ev => {
                #[cfg(debug_assertions)]
                if let Event::SessionIdle { session_id } = &ev
                    && std::env::var_os("OCDESK_VERIFY_ON_IDLE").is_some()
                {
                    self.verify_against_rest(session_id.clone(), cx);
                }
                // Verification hook for dev/teardown-test.sh: quit through
                // the normal on_app_quit path once a turn finishes.
                if matches!(ev, Event::SessionIdle { .. })
                    && std::env::var_os("OCDESK_EXIT_ON_IDLE").is_some()
                {
                    cx.quit();
                }
                self.store.update(cx, |s, cx| s.apply(ev, cx));
            }
        }
    }

    /// Debug check (OCDESK_VERIFY_ON_IDLE): the streamed state of a loaded
    /// conversation must equal the server's REST history once it goes idle.
    #[cfg(debug_assertions)]
    fn verify_against_rest(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let sid = session_id.clone();
        // The server persists the final text slightly after `session.idle`
        // (seen on abort), so give it a moment before comparing.
        let fut = async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            api.messages(&sid).await
        };
        self.run(cx, fut, move |this, res, cx| {
            let Ok(history) = res else { return };
            let store = this.store.read(cx);
            let Some(conv) = store.conversations.get(&session_id).filter(|c| c.loaded) else {
                return;
            };
            let flatten = |parts: &[opencode_codes::protocol_generated::types::Part]| -> String {
                parts
                    .iter()
                    .filter_map(crate::store::part_text)
                    .collect::<Vec<_>>()
                    .join("|")
            };
            let local: Vec<String> = conv.messages.iter().map(|m| flatten(&m.parts)).collect();
            let remote: Vec<String> = history.iter().map(|m| flatten(&m.parts)).collect();
            if local == remote {
                tracing::info!(%session_id, messages = local.len(), "verify: store matches REST");
            } else {
                tracing::error!(%session_id, ?local, ?remote, "verify: store MISMATCH with REST");
            }
        });
    }

    /// After every (re)connect frames may have been missed: re-fetch the
    /// session list, the busy states, and the selected conversation.
    fn resync(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        self.store
            .update(cx, |s, _| s.invalidate_except(selected.as_deref()));
        self.refresh_sessions(cx);
        if let Some(id) = selected {
            self.load_messages(id, cx);
        }
    }

    // ── intents ──────────────────────────────────────────────────────────

    fn api(&self, cx: &mut Context<Self>) -> Option<Api> {
        if self.api.is_none() {
            self.notice("opencode is not running", cx);
        }
        self.api.clone()
    }

    fn notice(&self, msg: impl Into<String>, cx: &mut Context<Self>) {
        let msg = msg.into();
        tracing::warn!("{msg}");
        self.store.update(cx, |s, cx| {
            s.notice = Some(msg);
            cx.notify();
        });
    }

    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        self.run(
            cx,
            async move { api.list_sessions().await },
            |this, res, cx| match res {
                Ok(list) => this.store.update(cx, |s, cx| s.set_sessions(list, cx)),
                Err(e) => this.notice(format!("Could not list sessions: {e:#}"), cx),
            },
        );
    }

    fn load_messages(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let sid = session_id.clone();
        self.run(
            cx,
            async move { api.messages(&sid).await },
            move |this, res, cx| match res {
                Ok(history) => this
                    .store
                    .update(cx, |s, cx| s.reconcile(&session_id, history, cx)),
                Err(e) => this.notice(format!("Could not load messages: {e:#}"), cx),
            },
        );
    }

    pub fn select_session(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.select(id.clone(), cx));
        if let Some(id) = id {
            self.load_messages(id, cx);
        }
    }

    /// Codex-style: "New session" just clears the selection; the session is
    /// created on the first prompt so empty sessions don't pile up.
    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        self.select_session(None, cx);
    }

    pub fn delete_session(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let sid = id.clone();
        self.run(
            cx,
            async move { api.delete_session(&sid).await },
            move |this, res, cx| match res {
                Ok(_) => this.store.update(cx, |s, cx| {
                    s.apply(Event::SessionDeleted { session_id: id }, cx)
                }),
                Err(e) => this.notice(format!("Could not delete session: {e:#}"), cx),
            },
        );
    }

    pub fn rename_session(&mut self, id: String, title: String, cx: &mut Context<Self>) {
        let title = title.trim().to_string();
        if title.is_empty() {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.rename_session(&id, &title).await },
            |this, res, cx| match res {
                Ok(session) => this
                    .store
                    .update(cx, |s, cx| s.apply(Event::SessionUpserted(session), cx)),
                Err(e) => this.notice(format!("Could not rename session: {e:#}"), cx),
            },
        );
    }

    /// Send a prompt to the selected session, creating one first if needed.
    pub fn send_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let selected = self.store.read(cx).selected.clone();
        if let Some(sid) = &selected {
            // Flip to busy right away so the composer shows Stop; the server
            // confirms with session.status within milliseconds.
            self.store.update(cx, |s, cx| {
                s.clear_error(sid);
                s.set_status(sid, RunState::Busy, cx);
            });
        }
        self.run(
            cx,
            async move {
                let (sid, created) = match selected {
                    Some(sid) => (sid, None),
                    None => {
                        let s = api.create_session().await?;
                        (s.id.clone(), Some(s))
                    }
                };
                api.prompt_text(&sid, &text).await?;
                Ok((sid, created))
            },
            |this, res, cx| match res {
                Ok((sid, Some(session))) => {
                    this.store.update(cx, |s, cx| {
                        s.apply(Event::SessionUpserted(session), cx);
                        s.set_status(&sid, RunState::Busy, cx);
                    });
                    this.select_session(Some(sid), cx);
                }
                Ok(_) => {}
                Err(e) => {
                    if let Some(sid) = this.store.read(cx).selected.clone() {
                        this.store
                            .update(cx, |s, cx| s.set_status(&sid, RunState::Idle, cx));
                    }
                    this.notice(format!("Could not send prompt: {e:#}"), cx);
                }
            },
        );
    }

    pub fn abort(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        self.run(cx, async move { api.abort(&sid).await }, |this, res, cx| {
            if let Err(e) = res {
                this.notice(format!("Could not abort: {e:#}"), cx);
            }
        });
    }

    /// Debug/verification hook: send `text` once connected (OCDESK_PROBE_PROMPT).
    pub fn probe_on_connect(&mut self, text: String, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let mut text = Some(text);
        cx.observe(&store, move |this, store, cx| {
            if store.read(cx).connection() == Connection::Connected
                && let Some(t) = text.take()
            {
                this.send_prompt(t, cx);
            }
        })
        .detach();
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stop_pump();
    }
}

pub fn default_working_dir() -> Result<PathBuf> {
    Ok(match std::env::var_os("OCDESK_DIR") {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()?,
    })
}
