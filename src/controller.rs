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

use crate::api::model::{FsEntry, ModelRef, PermissionDecision, PromptInput};
use crate::api::{Api, Envelope, Event};
use crate::settings::Settings;
use crate::sidecar::{self, SidecarConfig};
use crate::store::{Catalog, Connection, Store};

/// Give up auto-restarting after this many crashes within [`CRASH_WINDOW`].
const MAX_QUICK_RESTARTS: u32 = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// How long a transient notice stays visible before clearing itself.
const NOTICE_LINGER: Duration = Duration::from_secs(10);
/// Retries for reads that can race a fresh sidecar's lazy location
/// registration (404 `LocationNotFoundError`).
const LOCATION_RETRIES: usize = 3;
const LOCATION_BACKOFF: Duration = Duration::from_millis(400);
/// `model.updated`/`agent.updated` arrive in bursts (startup, config reload).
const CATALOG_DEBOUNCE: Duration = Duration::from_millis(300);
/// Quiet period before bursty settings changes (resizes, drags) are written.
const SETTINGS_DEBOUNCE: Duration = Duration::from_millis(500);

/// A 404 on `permission`/`form` reads right after startup usually means the
/// session's location has not been registered server-side yet — retry, don't
/// bother the user.
fn is_location_404(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("404") || s.contains("LocationNotFoundError")
}

async fn with_location_retry<F, T>(mut f: F) -> Result<T>
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Err(e) if attempt < LOCATION_RETRIES && is_location_404(&e) => {
                tracing::warn!(
                    "location read not ready yet ({}), retrying ({}/{})",
                    e,
                    attempt + 1,
                    LOCATION_RETRIES
                );
                attempt += 1;
                tokio::time::sleep(LOCATION_BACKOFF * attempt as u32).await;
            }
            res => return res,
        }
    }
}

pub struct Controller {
    rt: tokio::runtime::Handle,
    store: Entity<Store>,
    settings: Settings,
    api: Option<Api>,
    /// Bumped on every sidecar (re)start so stale callbacks can be ignored.
    generation: u64,
    connection_epoch: u64,
    diff_requests: std::collections::HashMap<String, u64>,
    pump: Option<(tokio::task::AbortHandle, Task<()>)>,
    exit_watch: Option<Task<()>>,
    recent_crashes: Vec<Instant>,
    /// Bumped per notice so a stale auto-clear timer can't hide a newer one.
    notice_seq: u64,
    /// Bumped per catalog refresh request, to debounce bursts.
    catalog_seq: u64,
    /// Offline UI demo (`OCDESK_DEMO`): no sidecar; prompts get synthetic replies.
    demo: bool,
    /// Session to reopen once the first session list arrives (last one open
    /// when ocdesk quit). Taken on first use.
    restore_session: Option<String>,
    /// Bumped per debounced settings save.
    save_seq: u64,
}

impl Controller {
    pub fn new(
        rt: tokio::runtime::Handle,
        store: Entity<Store>,
        mut settings: Settings,
        initial_folder: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        // An explicit folder argument means "start there", not "where I was".
        let restore_session = if initial_folder.is_none() {
            settings.last_session.clone()
        } else {
            None
        };
        if let Some(f) = initial_folder
            && !settings.folders.contains(&f)
        {
            settings.folders.insert(0, f);
        }
        let (folders, collapsed) = (settings.folders.clone(), settings.collapsed.clone());
        let (draft_model, draft_agent) = (settings.draft_model.clone(), settings.draft_agent.clone());
        store.update(cx, |s, cx| {
            s.collapsed = collapsed.into_iter().collect();
            s.draft_model = draft_model;
            s.draft_agent = draft_agent;
            s.set_folders(folders, cx);
        });
        let mut this = Self {
            rt,
            store,
            settings,
            api: None,
            generation: 0,
            connection_epoch: 0,
            diff_requests: Default::default(),
            pump: None,
            exit_watch: None,
            recent_crashes: Vec::new(),
            notice_seq: 0,
            catalog_seq: 0,
            demo: false,
            restore_session,
            save_seq: 0,
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

    /// Read-only view of the persisted settings (window placement etc.).
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Change settings and save them after a short quiet period, for values
    /// that change in bursts (window resizes, sidebar drags).
    pub fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Settings),
    ) {
        let before = self.settings.clone();
        f(&mut self.settings);
        if self.settings == before {
            return;
        }
        self.save_seq += 1;
        let seq = self.save_seq;
        let exec = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            exec.timer(SETTINGS_DEBOUNCE).await;
            let _ = this.update(cx, |this, _| {
                if this.save_seq == seq {
                    this.save_settings();
                }
            });
        })
        .detach();
    }

    /// Write pending settings now (on quit).
    pub fn flush_settings(&mut self) {
        self.save_seq += 1;
        if let Err(e) = self.settings.save() {
            tracing::warn!("could not save settings: {e:#}");
        }
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
        self.store.update(cx, |s, cx| {
            s.permission_sending.clear();
            s.form_sending.clear();
            s.diff_loading.clear();
            cx.notify();
        });
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
                        "opencode 2.x is not installed or not on PATH. \
                         Install it with: {}",
                        sidecar::INSTALL_HINT
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
                self.connection_epoch += 1;
                #[cfg(debug_assertions)]
                if std::env::var_os("OCDESK_DEBUG_PANIC").is_some() {
                    panic!("OCDESK_DEBUG_PANIC: deliberate panic to test sidecar teardown");
                }
                self.store
                    .update(cx, |s, cx| s.set_connection(Connection::Connected, cx));
                self.resync(cx);
            }
            Event::Disconnected => {
                self.connection_epoch += 1;
                self.store.update(cx, |s, cx| {
                    s.permission_sending.clear();
                    s.form_sending.clear();
                    cx.notify();
                });
                self.store.update(cx, |s, cx| {
                    if s.connection() == Connection::Connected {
                        s.set_connection(Connection::Reconnecting, cx);
                    }
                });
            }
            Event::CatalogChanged => self.schedule_catalog_refresh(cx),
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
        self.refresh_permissions(_session_id.clone(), cx);
        self.refresh_forms(_session_id.clone(), cx);
        if self.store.read(cx).diffs.contains_key(&_session_id) {
            self.refresh_diff(_session_id.clone(), cx);
        }
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
        let mut pending_sessions: std::collections::HashSet<String> =
            self.store.read(cx).permissions.keys().cloned().collect();
        pending_sessions.extend(self.store.read(cx).forms.keys().cloned());
        if let Some(id) = selected {
            pending_sessions.insert(id.clone());
            self.load_messages(id.clone(), cx);
            if self.store.read(cx).diffs.contains_key(&id) {
                self.refresh_diff(id, cx);
            }
        }
        for sid in pending_sessions {
            self.refresh_permissions(sid.clone(), cx);
            self.refresh_forms(sid, cx);
        }
        // Prime every folder's location: a fresh sidecar registers a project
        // only when the directory is first mentioned, and until then reads for
        // that location's sessions 404 (LocationNotFoundError).
        let folders = self.store.read(cx).folders.clone();
        if let Some(api) = self.api.clone() {
            self.run(
                cx,
                async move {
                    for f in &folders {
                        let _ = api.location(f).await;
                    }
                    Ok::<(), anyhow::Error>(())
                },
                |this, _res, cx| this.refresh_catalogs(cx),
            );
        }
    }

    // ── models and agents ────────────────────────────────────────────────

    fn schedule_catalog_refresh(&mut self, cx: &mut Context<Self>) {
        self.catalog_seq += 1;
        let seq = self.catalog_seq;
        let exec = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            exec.timer(CATALOG_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.catalog_seq == seq {
                    this.refresh_catalogs(cx);
                }
            });
        })
        .detach();
    }

    /// Re-read models, default model and agents for every folder.
    fn refresh_catalogs(&mut self, cx: &mut Context<Self>) {
        for folder in self.store.read(cx).folders.clone() {
            self.refresh_catalog(folder, cx);
        }
    }

    fn refresh_catalog(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let generation = self.generation;
        let dir = folder.clone();
        self.run(
            cx,
            async move {
                let (models, default_model, agents) =
                    futures::join!(api.models(&dir), api.default_model(&dir), api.agents(&dir));
                Ok(Catalog {
                    models: models?,
                    // No default configured is not an error.
                    default_model: default_model.unwrap_or_else(|e| {
                        tracing::warn!("no default model for {}: {e:#}", dir.display());
                        None
                    }),
                    agents: agents?,
                })
            },
            move |this, res: Result<Catalog>, cx| {
                if this.generation != generation {
                    return;
                }
                match res {
                    Ok(c) => this.store.update(cx, |s, cx| s.set_catalog(folder, c, cx)),
                    Err(e) => tracing::warn!(
                        "could not read models/agents for {}: {e:#}",
                        folder.display()
                    ),
                }
            },
        );
    }

    /// Pick the model (and variant) for the selected session, or for the
    /// next new session when nothing is selected.
    pub fn choose_model(&mut self, model: ModelRef, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.set_draft_choice(Some(model.clone()), None, cx)
        });
        let draft = Some(model.clone());
        self.update_settings(cx, |s| s.draft_model = draft);
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let previous = self
            .store
            .read(cx)
            .session(&sid)
            .and_then(|s| s.model.clone());
        // Optimistic; the stream confirms with session.model.selected.
        self.store.update(cx, |s, cx| {
            s.apply(
                Envelope::bare(Event::ModelSelected {
                    session_id: sid.clone(),
                    model: model.clone(),
                }),
                cx,
            )
        });
        let target = sid.clone();
        self.run(
            cx,
            async move { api.switch_model(&sid, &model).await },
            move |this, res, cx| {
                let Err(e) = res else { return };
                this.notice(format!("Could not switch model: {e:#}"), cx);
                // Roll back the optimistic choice.
                if let Some(prev) = previous {
                    this.store.update(cx, |s, cx| {
                        s.apply(
                            Envelope::bare(Event::ModelSelected {
                                session_id: target,
                                model: prev,
                            }),
                            cx,
                        )
                    });
                }
            },
        );
    }

    /// Pick the primary agent for the selected session, or the next new one.
    pub fn choose_agent(&mut self, agent: String, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.set_draft_choice(None, Some(agent.clone()), cx)
        });
        let draft = Some(agent.clone());
        self.update_settings(cx, |s| s.draft_agent = draft);
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        self.store.update(cx, |s, cx| {
            s.apply(
                Envelope::bare(Event::AgentSelected {
                    session_id: sid.clone(),
                    agent: agent.clone(),
                }),
                cx,
            )
        });
        self.run(
            cx,
            async move { api.switch_agent(&sid, &agent).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not switch agent: {e:#}"), cx);
                }
            },
        );
    }

    /// Fuzzy file search for `@` mentions. `None` when opencode isn't running.
    pub fn find_files(
        &self,
        folder: PathBuf,
        query: String,
    ) -> Option<oneshot::Receiver<Result<Vec<FsEntry>>>> {
        let api = self.api.clone()?;
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(api.find_files(&folder, &query, 20).await);
        });
        Some(rx)
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

    fn api(&mut self, cx: &mut Context<Self>) -> Option<Api> {
        if self.api.is_none() {
            self.notice("opencode is not running", cx);
        }
        self.api.clone()
    }

    /// Transient warning in the conversation's notice bar. Auto-clears after
    /// [`NOTICE_LINGER`] so a stale error never sticks around.
    fn notice(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        let msg = msg.into();
        tracing::warn!("{msg}");
        self.notice_seq += 1;
        let seq = self.notice_seq;
        self.store.update(cx, |s, cx| {
            s.notice = Some(msg);
            cx.notify();
        });
        let exec = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            exec.timer(NOTICE_LINGER).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice_seq != seq {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    if s.notice.is_some() {
                        s.notice = None;
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        self.run(
            cx,
            async move { api.list_sessions().await },
            |this, res, cx| match res {
                Ok(list) => {
                    let ids: Vec<_> = list.iter().map(|s| s.id.clone()).collect();
                    this.store.update(cx, |s, cx| s.set_sessions(list, cx));
                    for id in ids {
                        this.refresh_permissions(id, cx);
                    }
                    this.restore_last_session(cx);
                }
                Err(e) => this.notice(format!("Could not list sessions: {e:#}"), cx),
            },
        );
    }

    /// Fetch a session's history. Failures are shown in the conversation
    /// with a Retry button (see `ConversationView`), not as a notice.
    pub fn load_messages(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        self.store.update(cx, |s, cx| {
            if s.load_errors.remove(&session_id).is_some() {
                cx.notify();
            }
        });
        let sid = session_id.clone();
        self.run(
            cx,
            async move { api.messages(&sid).await },
            move |this, res, cx| match res {
                Ok(history) => this
                    .store
                    .update(cx, |s, cx| s.reconcile(&session_id, history, cx)),
                Err(e) => {
                    tracing::warn!(%session_id, "could not load messages: {e:#}");
                    this.store.update(cx, |s, cx| {
                        s.load_errors.insert(session_id, format!("{e:#}"));
                        cx.notify();
                    });
                }
            },
        );
    }

    pub fn refresh_permissions(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let (generation, epoch) = (self.generation, self.connection_epoch);
        let revision = self.store.read(cx).permission_revision(&sid);
        let id = sid.clone();
        self.run(
            cx,
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    Box::pin(async move { api.permissions(&id).await })
                })
                .await
            },
            move |this, res, cx| {
                if this.generation != generation || this.connection_epoch != epoch {
                    return;
                }
                match res {
                    Ok(requests) => {
                        // A live asked/replied event (or newer snapshot) wins over this read.
                        if this.store.read(cx).permission_revision(&sid) == revision {
                            this.store
                                .update(cx, |s, cx| s.set_permissions(&sid, requests, cx));
                        } else {
                            this.refresh_permissions(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!("could not load permission requests: {e:#}")
                    }
                    Err(e) => this.notice(format!("Could not load permission requests: {e:#}"), cx),
                }
            },
        );
    }

    pub fn refresh_forms(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let (generation, epoch) = (self.generation, self.connection_epoch);
        let revision = self.store.read(cx).form_revision(&sid);
        let id = sid.clone();
        self.run(
            cx,
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    Box::pin(async move { api.forms(&id).await })
                })
                .await
            },
            move |this, res, cx| {
                if this.generation != generation || this.connection_epoch != epoch {
                    return;
                }
                match res {
                    Ok(forms) => {
                        // A live created/replied event (or newer snapshot) wins over this read.
                        if this.store.read(cx).form_revision(&sid) == revision {
                            this.store.update(cx, |s, cx| s.set_forms(&sid, forms, cx));
                        } else {
                            this.refresh_forms(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!("could not load question forms: {e:#}")
                    }
                    Err(e) => this.notice(format!("Could not load question forms: {e:#}"), cx),
                }
            },
        );
    }

    pub fn reply_form(
        &mut self,
        sid: String,
        fid: String,
        answer: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else { return };
        let key = (sid.clone(), fid.clone());
        let store = self.store.read(cx);
        let exists = store
            .forms
            .get(&sid)
            .is_some_and(|fs| fs.iter().any(|f| f.id == fid));
        if !exists
            || store.form_sending.contains(&key)
            || store.connection() != Connection::Connected
        {
            return;
        }
        let (generation, epoch) = (self.generation, self.connection_epoch);
        self.store.update(cx, |s, cx| {
            s.form_sending.insert(key.clone());
            s.form_errors.remove(&key);
            cx.notify();
        });
        let (session, form) = (sid.clone(), fid.clone());
        self.run(
            cx,
            async move { api.reply_form(&session, &form, &answer).await },
            move |this, res, cx| {
                if this.generation != generation || this.connection_epoch != epoch {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.form_sending.remove(&key);
                    match res {
                        Ok(()) => s.apply(
                            Envelope::bare(Event::FormReplied {
                                session_id: sid.clone(),
                                form_id: fid.clone(),
                            }),
                            cx,
                        ),
                        Err(e) => {
                            // Keep the form visible and retryable after an HTTP failure.
                            if s.forms
                                .get(&sid)
                                .is_some_and(|fs| fs.iter().any(|f| f.id == fid))
                            {
                                s.form_errors.insert(key, format!("Could not reply: {e:#}"));
                            }
                            cx.notify();
                        }
                    }
                });
                this.refresh_forms(sid, cx);
            },
        );
    }

    /// Record one answer in a form's draft (option picked, box toggled, text
    /// typed). Pure UI state; the server sees it only on submit.
    pub fn set_form_answer(
        &self,
        form_id: String,
        key: String,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |s, cx| {
            s.set_form_draft(&form_id, &key, value, cx);
        });
    }

    /// The committed draft answers of the selected form.
    pub fn form_draft(&self, form_id: String, cx: &Context<Self>) -> serde_json::Value {
        self.store.read(cx).form_draft(&form_id)
    }

    pub fn cancel_form(&mut self, sid: String, fid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let key = (sid.clone(), fid.clone());
        let store = self.store.read(cx);
        let exists = store
            .forms
            .get(&sid)
            .is_some_and(|fs| fs.iter().any(|f| f.id == fid));
        if !exists || store.form_sending.contains(&key) {
            return;
        }
        let (generation, epoch) = (self.generation, self.connection_epoch);
        self.store.update(cx, |s, cx| {
            s.form_sending.insert(key.clone());
            cx.notify();
        });
        let (session, form) = (sid.clone(), fid.clone());
        self.run(
            cx,
            async move { api.cancel_form(&session, &form).await },
            move |this, res, cx| {
                if this.generation != generation || this.connection_epoch != epoch {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.form_sending.remove(&key);
                    match res {
                        Ok(()) => s.apply(
                            Envelope::bare(Event::FormCancelled {
                                session_id: sid.clone(),
                                form_id: fid.clone(),
                            }),
                            cx,
                        ),
                        Err(e) => {
                            let msg = format!("Could not cancel: {e:#}");
                            tracing::warn!("{msg}");
                            if s.forms
                                .get(&sid)
                                .is_some_and(|fs| fs.iter().any(|f| f.id == fid))
                            {
                                s.form_errors.insert(key, msg);
                            }
                            cx.notify();
                        }
                    }
                });
                this.refresh_forms(sid, cx);
            },
        );
    }

    pub fn reply_permission(
        &mut self,
        sid: String,
        rid: String,
        decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else { return };
        let key = (sid.clone(), rid.clone());
        let store = self.store.read(cx);
        let exists = store
            .permissions
            .get(&sid)
            .is_some_and(|rs| rs.iter().any(|r| r.id == rid));
        if !exists
            || store.permission_sending.contains(&key)
            || store.connection() != Connection::Connected
        {
            return;
        }
        let (generation, epoch) = (self.generation, self.connection_epoch);
        self.store.update(cx, |s, cx| {
            s.permission_sending.insert(key.clone());
            s.permission_errors.remove(&key);
            cx.notify();
        });
        let (session, request) = (sid.clone(), rid.clone());
        self.run(
            cx,
            async move { api.reply_permission(&session, &request, decision).await },
            move |this, res, cx| {
                if this.generation != generation || this.connection_epoch != epoch {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.permission_sending.remove(&key);
                    match res {
                        Ok(()) => s.apply(
                            Envelope::bare(Event::PermissionReplied {
                                session_id: sid.clone(),
                                request_id: rid,
                            }),
                            cx,
                        ),
                        Err(e) => {
                            // Keep the request visible and retryable after an HTTP failure.
                            if s.permissions
                                .get(&sid)
                                .is_some_and(|rs| rs.iter().any(|r| r.id == rid))
                            {
                                s.permission_errors
                                    .insert(key, format!("Could not reply: {e:#}"));
                            }
                            cx.notify();
                        }
                    }
                });
                this.refresh_permissions(sid, cx);
            },
        );
    }

    pub fn refresh_diff(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let generation = self.generation;
        let request = self.diff_requests.entry(sid.clone()).or_default();
        *request += 1;
        let request = *request;
        self.store.update(cx, |s, cx| {
            s.diff_loading.insert(sid.clone());
            s.diff_errors.remove(&sid);
            cx.notify();
        });
        let id = sid.clone();
        self.run(
            cx,
            async move { api.diff(&id).await },
            move |this, res, cx| {
                if this.generation != generation || this.diff_requests.get(&sid) != Some(&request) {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.diff_loading.remove(&sid);
                    match res {
                        Ok(files) => {
                            s.diffs.insert(sid, files);
                        }
                        Err(e) => {
                            s.diff_errors
                                .insert(sid, format!("Could not load changes: {e:#}"));
                        }
                    }
                    cx.notify();
                });
            },
        );
    }

    /// Reopen the session that was open at quit, once, if it still exists
    /// and the user hasn't picked something else meanwhile.
    fn restore_last_session(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.restore_session.take() else {
            return;
        };
        let store = self.store.read(cx);
        if store.selected.is_none() && store.session(&id).is_some() {
            self.select_session(Some(id), cx);
        }
    }

    /// Keyboard navigation, see [`Store::adjacent_session`].
    pub fn select_adjacent(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(id) = self.store.read(cx).adjacent_session(delta) {
            self.select_session(Some(id), cx);
        }
    }

    pub fn select_session(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.restore_session = None;
        self.store.update(cx, |s, cx| s.select(id.clone(), cx));
        let last = id.clone();
        self.update_settings(cx, |s| s.last_session = last);
        if let Some(id) = id {
            self.refresh_permissions(id.clone(), cx);
            self.refresh_forms(id.clone(), cx);
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
            this.refresh_catalog(folder.clone(), cx);
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
    /// folder first if nothing is selected. A new session gets the draft
    /// model/agent before its first prompt.
    pub fn send_prompt(&mut self, mut input: PromptInput, cx: &mut Context<Self>) {
        input.text = input.text.trim_end().to_string();
        if input.text.trim().is_empty() {
            return;
        }
        if self.demo {
            crate::demo::stream_reply(self.store.clone(), input.text, cx);
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let store = self.store.read(cx);
        let selected = store.selected.clone();
        let (draft_model, draft_agent) = (store.draft_model.clone(), store.draft_agent.clone());
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
                        let mut s = api.create_session(&folder).await?;
                        if let Some(m) = &draft_model {
                            api.switch_model(&s.id, m).await?;
                            s.model = Some(m.clone());
                        }
                        if let Some(a) = &draft_agent {
                            api.switch_agent(&s.id, a).await?;
                            s.agent = Some(a.clone());
                        }
                        (s.id.clone(), Some(s))
                    }
                };
                api.prompt(&sid, &input).await?;
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

    /// Keyboard stop: only interrupts when the selected session is running,
    /// so a stray shortcut never sends a request (or shows a notice).
    pub fn abort_if_busy(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if store.selected.as_ref().is_some_and(|id| store.is_busy(id)) {
            self.abort(cx);
        }
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
                this.send_prompt(
                    PromptInput {
                        text: t,
                        ..Default::default()
                    },
                    cx,
                );
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
