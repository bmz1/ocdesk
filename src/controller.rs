//! Turns user intents into API calls and API results into store updates.
//!
//! Owns the sidecar lifecycle (start, crash detection, auto-restart), the
//! SSE pump, and the folder list. One sidecar serves every folder:
//! sessions are pinned to a location (directory) and `/api/event` covers all
//! locations. All network work runs on the Tokio runtime; results come back
//! to the GPUI thread through channels awaited in `cx.spawn` tasks.

use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui_kit::{Context, Entity, Task};

use crate::api::{Api, Envelope, Event};
use crate::settings::Settings;
use crate::sidecar::{self, SidecarConfig};
use crate::store::{Connection, Store};

/// Give up auto-restarting after this many crashes within [`CRASH_WINDOW`].
const MAX_QUICK_RESTARTS: u32 = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

pub struct Controller {
    rt: tokio::runtime::Handle,
    store: Entity<Store>,
    settings: Settings,
    api: Option<Api>,
    /// Bumped on every sidecar (re)start so stale callbacks can be ignored.
    generation: u64,
    pump: Option<(tokio::task::AbortHandle, Task<()>)>,
    exit_watch: Option<Task<()>>,
    recent_crashes: Vec<Instant>,
    /// Offline UI demo (`OCDESK_DEMO`): no sidecar; prompts get synthetic replies.
    demo: bool,
}

impl Controller {
    pub fn new(
        rt: tokio::runtime::Handle,
        store: Entity<Store>,
        mut settings: Settings,
        initial_folder: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(f) = initial_folder
            && !settings.folders.contains(&f)
        {
            settings.folders.insert(0, f);
        }
        let (folders, collapsed) = (settings.folders.clone(), settings.collapsed.clone());
        store.update(cx, |s, cx| {
            s.collapsed = collapsed.into_iter().collect();
            s.set_folders(folders, cx);
        });
        let mut this = Self {
            rt,
            store,
            settings,
            api: None,
            generation: 0,
            pump: None,
            exit_watch: None,
            recent_crashes: Vec::new(),
            demo: false,
        };
        this.save_settings();
        if let Some(turns) = crate::demo::turns() {
            this.demo = true;
            crate::demo::start(&this.store, turns, cx);
        } else {
            this.start_sidecar(cx);
        }
        this
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

    fn save_settings(&self) {
        let settings = self.settings.clone();
        self.rt.spawn_blocking(move || {
            if let Err(e) = settings.save() {
                tracing::warn!("could not save settings: {e:#}");
            }
        });
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

        // Every session carries its own location, so the sidecar's own
        // directory doesn't matter.
        let working_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
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
                let started = res.and_then(|(sc, exit_rx)| Ok((Api::new(&sc)?, sc, exit_rx)));
                let (api, sc, exit_rx) = match started {
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
                    s.set_active(vec![], cx);
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
            while let Some(env) = rx.next().await {
                if this.update(cx, |this, cx| this.on_event(env, cx)).is_err() {
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

    fn on_event(&mut self, env: Envelope, cx: &mut Context<Self>) {
        match env.event {
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
            _ => {
                let turn_over = match &env.event {
                    Event::ExecutionEnded { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                };
                self.store.update(cx, |s, cx| s.apply(env, cx));
                if let Some(session_id) = turn_over {
                    self.on_turn_over(session_id, cx);
                }
            }
        }
    }

    /// A turn finished (`session.execution.succeeded|failed|interrupted`).
    fn on_turn_over(&mut self, _session_id: String, cx: &mut Context<Self>) {
        #[cfg(debug_assertions)]
        if std::env::var_os("OCDESK_VERIFY_ON_IDLE").is_some() {
            self.verify_against_rest(_session_id, cx);
        }
        // Verification hook for dev/teardown-test.sh: quit through the normal
        // on_app_quit path once a turn finishes.
        if std::env::var_os("OCDESK_EXIT_ON_IDLE").is_some() {
            cx.quit();
        }
    }

    /// After every (re)connect frames may have been missed: re-fetch the
    /// session list, busy states, and the selected conversation.
    fn resync(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        self.store
            .update(cx, |s, _| s.invalidate_except(selected.as_deref()));
        self.refresh_sessions(cx);
        if let Some(api) = self.api.clone() {
            self.run(
                cx,
                async move { api.active().await },
                |this, res, cx| match res {
                    Ok(active) => this.store.update(cx, |s, cx| s.set_active(active, cx)),
                    Err(e) => tracing::warn!("could not read active sessions: {e:#}"),
                },
            );
        }
        if let Some(id) = selected {
            self.load_messages(id, cx);
        }
    }

    /// Debug check (OCDESK_VERIFY_ON_IDLE): the streamed state of a loaded
    /// conversation must equal the server's REST projection after a turn.
    #[cfg(debug_assertions)]
    fn verify_against_rest(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let sid = session_id.clone();
        // Give the projection a moment after the terminal event.
        let fut = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            api.messages(&sid).await
        };
        self.run(cx, fut, move |this, res, cx| {
            let Ok(history) = res else { return };
            let store = this.store.read(cx);
            let Some(conv) = store.conversations.get(&session_id).filter(|c| c.loaded) else { return };
            if conv.messages == history {
                tracing::info!(%session_id, messages = history.len(), "verify: store matches REST");
            } else {
                tracing::error!(%session_id, local = ?conv.messages, remote = ?history, "verify: store MISMATCH with REST");
            }
        });
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

    /// Codex-style: "New session" clears the selection and picks the folder;
    /// the session is created on the first prompt so empty sessions don't
    /// pile up.
    pub fn new_session(&mut self, folder: Option<PathBuf>, cx: &mut Context<Self>) {
        let folder = folder.or_else(|| self.store.read(cx).target_folder());
        self.select_session(None, cx);
        if let Some(f) = folder {
            self.store.update(cx, |s, cx| s.set_current_folder(f, cx));
        }
    }

    /// Add a folder (validated by the server as an opencode location).
    pub fn add_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let folder = std::fs::canonicalize(&folder).unwrap_or(folder);
        if !folder.is_dir() {
            self.notice(format!("{} is not a folder", folder.display()), cx);
            return;
        }
        let finish = |this: &mut Self, folder: PathBuf, cx: &mut Context<Self>| {
            if !this.settings.folders.contains(&folder) {
                this.settings.folders.push(folder.clone());
                this.save_settings();
            }
            let folders = this.settings.folders.clone();
            this.store.update(cx, |s, cx| {
                s.set_folders(folders, cx);
                s.collapsed.remove(&folder);
            });
            this.new_session(Some(folder), cx);
        };
        match self.api.clone() {
            Some(api) => {
                let f = folder.clone();
                self.run(
                    cx,
                    async move { api.location(&f).await },
                    move |this, res, cx| match res {
                        Ok(dir) => finish(this, dir, cx),
                        Err(e) => this.notice(
                            format!("opencode can't open {}: {e:#}", folder.display()),
                            cx,
                        ),
                    },
                );
            }
            None => finish(self, folder, cx),
        }
    }

    /// Hide a folder from the sidebar (its sessions are kept on the server).
    pub fn remove_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.settings.folders.retain(|f| f != &folder);
        self.settings.collapsed.retain(|f| f != &folder);
        self.save_settings();
        let folders = self.settings.folders.clone();
        let selected_in_folder = {
            let s = self.store.read(cx);
            s.selected
                .as_ref()
                .and_then(|id| s.session(id))
                .is_some_and(|s| s.directory == folder)
        };
        if selected_in_folder {
            self.select_session(None, cx);
        }
        self.store.update(cx, |s, cx| s.set_folders(folders, cx));
    }

    pub fn toggle_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.store
            .update(cx, |s, cx| s.toggle_collapsed(&folder, cx));
        self.settings.collapsed = self.store.read(cx).collapsed.iter().cloned().collect();
        self.save_settings();
    }

    pub fn delete_session(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let sid = id.clone();
        // The stream also confirms with session.deleted; applying it here
        // makes the row disappear without waiting.
        self.run(
            cx,
            async move { api.delete_session(&sid).await },
            move |this, res, cx| match res {
                Ok(()) => this.store.update(cx, |s, cx| {
                    s.apply(Envelope::bare(Event::SessionRemoved { session_id: id }), cx)
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
        // The server confirms with session.renamed on the stream.
        self.run(
            cx,
            async move { api.rename_session(&id, &title).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not rename session: {e:#}"), cx);
                }
            },
        );
    }

    /// Send a prompt to the selected session, creating one in the target
    /// folder first if nothing is selected.
    pub fn send_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.demo {
            crate::demo::stream_reply(self.store.clone(), text, cx);
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let store = self.store.read(cx);
        let selected = store.selected.clone();
        let Some(folder) = store.target_folder() else {
            self.notice("Add a folder first", cx);
            return;
        };
        if let Some(sid) = &selected {
            // Flip to busy right away so the composer shows Stop; the server
            // confirms with session.execution.started within milliseconds.
            self.store.update(cx, |s, cx| {
                s.clear_error(sid);
                s.set_busy(sid, true, cx);
            });
        }
        self.run(
            cx,
            async move {
                let (sid, created) = match selected {
                    Some(sid) => (sid, None),
                    None => {
                        let s = api.create_session(&folder).await?;
                        (s.id.clone(), Some(s))
                    }
                };
                api.prompt(&sid, &text).await?;
                Ok((sid, created))
            },
            |this, res, cx| match res {
                Ok((sid, Some(session))) => {
                    this.store.update(cx, |s, cx| {
                        s.apply(Envelope::bare(Event::SessionUpserted(session)), cx);
                        s.set_busy(&sid, true, cx);
                    });
                    this.select_session(Some(sid), cx);
                }
                Ok(_) => {}
                Err(e) => {
                    if let Some(sid) = this.store.read(cx).selected.clone() {
                        this.store.update(cx, |s, cx| s.set_busy(&sid, false, cx));
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
        self.run(
            cx,
            async move { api.interrupt(&sid).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not stop: {e:#}"), cx);
                }
            },
        );
    }

    /// Verification hook: send `text` in a new session once connected
    /// (OCDESK_PROBE_PROMPT).
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

/// The folder to open at startup: `ocdesk <folder>`, else `OCDESK_DIR`.
/// On first run only (no saved folders), the launch directory when started
/// from a terminal; an app bundle starts in `/`.
pub fn initial_folder(have_folders: bool) -> Option<PathBuf> {
    if let Some(arg) = std::env::args_os().nth(1) {
        return Some(PathBuf::from(arg));
    }
    if let Some(d) = std::env::var_os("OCDESK_DIR") {
        return Some(PathBuf::from(d));
    }
    if have_folders {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    (cwd != std::path::Path::new("/") && Some(&cwd) != dirs::home_dir().as_ref()).then_some(cwd)
}
