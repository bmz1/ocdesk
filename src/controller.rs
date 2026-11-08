//! Turns user intents into API calls and API results into store updates.
//!
//! Owns the sidecar lifecycle (start, crash detection, auto-restart), the
//! SSE pump, and the folder list. One sidecar serves every folder:
//! sessions are pinned to a location (directory) and `/api/event` covers all
//! locations. All network work runs on the Tokio runtime; results come back
//! to the GPUI thread through channels awaited in `cx.spawn` tasks.

mod features;
mod files;
mod github;
mod reads;

use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui_kit::{Context, Entity, Task};

use crate::api::model::{FsEntry, ModelRef, PermissionDecision, PromptInput};
use crate::api::{Api, Envelope, Event, HttpError};
use crate::domain::approvals::{PendingRequest, PendingRequests};
use crate::domain::choice::{ChoiceToken, OptimisticChoices};
use crate::domain::requests::{
    ConnectionEpoch, Issued, RequestClock, RequestScope, Revision, Revisions, ServerGeneration,
};
use crate::domain::supervisor::{CrashResponse, RestartBudget};
use crate::settings::Settings;
use crate::sidecar::{self, SidecarConfig};
use crate::store::{Catalog, Connection, Store, StoreEvent, SubmissionTarget, Tooling};

/// Re-reads of a snapshot that raced live events, before applying it anyway.
const SNAPSHOT_RETRIES: u32 = 4;

/// Give up auto-restarting after this many crashes within [`CRASH_WINDOW`].
const MAX_QUICK_RESTARTS: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// Back-off before an automatic restart.
const RESTART_DELAY: Duration = Duration::from_secs(1);
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

/// Built-in tools that never write to the working tree.
fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "grep"
            | "glob"
            | "list"
            | "websearch"
            | "webfetch"
            | "codesearch"
            | "todowrite"
            | "todoread"
            | "skill"
            | "question"
    )
}

fn is_location_404(e: &anyhow::Error) -> bool {
    HttpError::is_not_found(e)
}

// Startup may not have registered the session location yet. Retry typed 404s;
// a diagnostic containing "404" must not turn unrelated failures into retries.
async fn with_location_retry<F, Fut, T>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Err(e) if attempt < LOCATION_RETRIES && is_location_404(&e) => {
                tracing::warn!(attempt = attempt + 1, max_retries = LOCATION_RETRIES,
                    error = %e, "location read not ready; retrying");
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
    requests: RequestClock,
    grant_requests: u64,
    submission_seq: u64,
    /// Bumped per permission-mode change, so an older mode read is dropped.
    mode_requests: Revisions<String>,
    /// Bumped per live usage total, so an older REST total is not shown.
    usage_revision: Revisions<String>,
    /// Outstanding optimistic model/agent choices per session.
    model_choices: OptimisticChoices<ModelRef>,
    agent_choices: OptimisticChoices<String>,
    /// Connection epoch in which each session's event-maintained state
    /// (history, approvals, inbox, tasks) was last fetched.
    fetched_epoch: std::collections::HashMap<String, ConnectionEpoch>,
    /// The usage dialog asked for details while a summary was in flight.
    usage_details_pending: std::collections::HashSet<String>,
    attention_seen: std::collections::HashSet<String>,
    diff_requests: Revisions<PathBuf>,
    task_requests: Revisions<PathBuf>,
    pump: Option<(tokio::task::AbortHandle, Task<()>)>,
    exit_watch: Option<Task<()>>,
    /// Crash budget for automatic restarts (`domain::supervisor`).
    restarts: RestartBudget,
    /// Pending automatic restart after a crash. Owned: any (re)start replaces
    /// or clears it, so a stale timer can never start a second sidecar.
    restart_timer: Option<Task<()>>,
    /// Auto-clear timer of the current notice. Replacing it cancels the old
    /// timer, so a stale one can't hide a newer notice.
    notice_clear: Option<Task<()>>,
    /// Pending debounced catalog refresh; replaced (cancelled) per burst event.
    catalog_debounce: Option<Task<()>>,
    /// Offline UI demo (`OCDESK_DEMO`): no sidecar; prompts get synthetic replies.
    demo: bool,
    failed_submission: Option<(Submission, Option<String>, PathBuf)>,
    /// Session to reopen once the first session list arrives (last one open
    /// when ocdesk quit). Taken on first use.
    restore_session: Option<String>,
    /// Pending debounced settings save; replaced per change, dropped on flush.
    save_debounce: Option<Task<()>>,
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
        store.update(cx, |s, cx| {
            s.preferences = settings.clone();
            s.unread = settings.unread.clone();
            s.collapsed = collapsed.into_iter().collect();
            s.set_folders(folders, cx);
        });
        let mut this = Self {
            rt,
            store,
            settings,
            api: None,
            requests: RequestClock::default(),
            diff_requests: Default::default(),
            task_requests: Default::default(),
            mode_requests: Default::default(),
            model_choices: Default::default(),
            agent_choices: Default::default(),
            grant_requests: 0,
            submission_seq: 0,
            usage_revision: Default::default(),
            usage_details_pending: Default::default(),
            fetched_epoch: Default::default(),
            attention_seen: Default::default(),
            pump: None,
            exit_watch: None,
            restarts: RestartBudget::new(MAX_QUICK_RESTARTS, CRASH_WINDOW),
            restart_timer: None,
            notice_clear: None,
            catalog_debounce: None,
            demo: false,
            failed_submission: None,
            restore_session,
            save_debounce: None,
        };
        this.reset_draft_choices(cx);
        this.save_settings();
        this.rt
            .spawn_blocking(crate::ui::composer::prune_old_pastes);
        if let Some(turns) = crate::demo::turns() {
            this.demo = true;
            crate::demo::start(&this.store, turns, cx);
        } else {
            this.start_sidecar(cx);
            this.start_github(cx);
        }
        this
    }

    /// Run `fut` on the Tokio runtime and hand the result back on the UI thread.
    ///
    /// Unfenced: `done` runs even after a sidecar restart. Use it for
    /// mutations whose outcome is persisted server state (delete, rename,
    /// archive, worktrees, inbox) or for local work (opening an editor), where
    /// a completion stays true on the next server. Reads, and anything whose
    /// completion rolls back or shows transient state, use [`Self::run_on`].
    ///
    /// Both halves are detached on purpose: an admitted mutation cannot be
    /// cancelled, and the GPUI half holds only a weak controller handle, so it
    /// does nothing once the controller is gone.
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

    /// Where a request was issued, to drop its response if that's gone.
    fn issued(&self, scope: RequestScope) -> Issued {
        self.requests.issue(scope)
    }

    fn is_current(&self, issued: Issued) -> bool {
        self.requests.accepts(issued)
    }

    /// [`Self::run`], but `done` only runs while the sidecar (and, for
    /// per-connection reads, the SSE connection) it was issued on is current.
    fn run_on<T, F>(
        &self,
        cx: &mut Context<Self>,
        issued: Issued,
        fut: F,
        done: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        self.run(cx, fut, move |this, res, cx| {
            if this.is_current(issued) {
                done(this, res, cx);
            } else {
                tracing::debug!(?issued, generation = ?this.requests.generation(), epoch = ?this.requests.epoch(), "discarded obsolete request completion");
            }
        });
    }

    /// Re-issue a snapshot read that raced live events, with backoff.
    /// Returns false once [`SNAPSHOT_RETRIES`] is reached: the caller then
    /// applies what it has (busy subagents could otherwise keep a
    /// reconnect re-reading forever).
    fn retry_snapshot(
        &mut self,
        attempt: u32,
        cx: &mut Context<Self>,
        again: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) -> bool {
        if attempt >= SNAPSHOT_RETRIES {
            return false;
        }
        let issued = self.issued(RequestScope::Connection);
        let delay = Duration::from_millis(100 << attempt);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.is_current(issued) {
                    again(this, cx);
                }
            });
        })
        .detach();
        true
    }

    /// Change settings and save them after a short quiet period, for values
    /// that change in bursts (window resizes, sidebar drags).
    pub fn update_settings(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        let before = self.settings.clone();
        f(&mut self.settings);
        if self.settings == before {
            return;
        }
        self.store.update(cx, |s, cx| {
            s.preferences = self.settings.clone();
            cx.notify();
        });
        self.schedule_save(cx);
    }

    /// Layout-only settings (pane widths, window placement): persisted, but
    /// views keep their own copy, so no clone of everything and no store
    /// notify, which would redraw every view per pixel of a drag.
    pub fn update_layout(&mut self, cx: &mut Context<Self>, f: impl Fn(&mut Settings)) {
        f(&mut self.settings);
        self.store.update(cx, |s, _| f(&mut s.preferences));
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let exec = cx.background_executor().clone();
        // Dropping the previous GPUI task cancels its timer.
        self.save_debounce = Some(cx.spawn(async move |this, cx| {
            exec.timer(SETTINGS_DEBOUNCE).await;
            let _ = this.update(cx, |this, _| this.save_settings());
        }));
    }

    /// Write pending settings now (on quit).
    pub fn flush_settings(&mut self) {
        self.save_debounce = None;
        // The newest snapshot: it waits for any write in flight, then wins.
        if let Err(e) = self.settings.snapshot_for_save().write() {
            tracing::warn!(op = "save_settings", error = %format!("{e:#}"), "could not save settings");
        }
    }

    fn save_settings(&self) {
        // Numbered here, on the UI thread, so request order decides which
        // write sticks however the background writes interleave.
        let pending = self.settings.snapshot_for_save();
        self.rt.spawn_blocking(move || {
            if let Err(e) = pending.write() {
                tracing::warn!(op = "save_settings", error = %format!("{e:#}"), "could not save settings");
            }
        });
    }

    // ── sidecar lifecycle ────────────────────────────────────────────────

    /// Kill whatever is running and start a fresh sidecar.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.restarts.reset();
        sidecar::kill_sidecar();
        self.start_sidecar(cx);
    }

    fn start_sidecar(&mut self, cx: &mut Context<Self>) {
        self.restart_timer = None;
        self.requests.restart();
        self.fetched_epoch.clear();
        // Their completions are discarded with the old generation.
        self.model_choices.clear();
        self.agent_choices.clear();
        let generation = self.requests.generation();
        self.api = None;
        self.store.update(cx, |s, cx| {
            s.cancel_history_loads();
            s.submitting.clear();
            s.interrupting.clear();
            s.abandon_replies(cx);
            for dir in s.diff_status.abandon_all() {
                cx.emit(StoreEvent::DiffsChanged { dir });
            }
            s.mode_sending.clear();
            s.usage_status.abandon_all();
            s.grant_sending.clear();
            s.saved_grants_status.abandon();
            cx.notify();
        });
        self.stop_pump();
        self.exit_watch = None;
        self.store
            .update(cx, |s, cx| s.set_connection(Connection::Starting, cx));

        // Every session carries its own location, so the sidecar's own
        // directory doesn't matter.
        let working_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
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
                let started = res.and_then(|(sc, exit_rx)| Ok((Api::new(&sc)?, sc, exit_rx)));
                let (api, sc, exit_rx) = match started {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(op = "start_sidecar", ?generation, error = %format!("{e:#}"), "sidecar failed to start");
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
        generation: ServerGeneration,
        exit_rx: tokio::sync::oneshot::Receiver<sidecar::SidecarExit>,
        cx: &mut Context<Self>,
    ) {
        self.exit_watch = Some(cx.spawn(async move |this, cx| {
            let Ok(exit) = exit_rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.requests.generation() != generation {
                    return;
                }
                this.api = None;
                this.stop_pump();
                let response = this.restarts.crashed(Instant::now());
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
                match response {
                    CrashResponse::Restart { crashes } => {
                        tracing::warn!(op = "start_sidecar", ?generation, crashes, code = ?exit.code, "restarting sidecar automatically");
                        this.restart_timer = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(RESTART_DELAY).await;
                            let _ = this.update(cx, |this, cx| {
                                // This task is the timer: let it finish instead
                                // of dropping (cancelling) itself mid-poll.
                                if let Some(me) = this.restart_timer.take() {
                                    me.detach();
                                }
                                this.start_sidecar(cx);
                            });
                        }));
                    }
                    CrashResponse::GiveUp { crashes } => {
                        tracing::error!(op = "start_sidecar", ?generation, crashes, code = ?exit.code, "sidecar keeps crashing; not restarting");
                    }
                }
            });
        }));
    }

    fn start_pump(&mut self, api: Api, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::channel(32);
        let handle = self
            .rt
            .spawn(async move { api.run_events(tx).await })
            .abort_handle();
        let task = cx.spawn(async move |this, cx| {
            while let Some(env) = rx.next().await {
                // Fold everything already queued in one update, so a burst
                // of streamed deltas flushes effects (and redraws) once.
                let mut batch = vec![env];
                while batch.len() < 256
                    && let Ok(env) = rx.try_recv()
                {
                    batch.push(env);
                }
                let applied = this.update(cx, |this, cx| {
                    for env in batch {
                        this.on_event(env, cx);
                    }
                });
                if applied.is_err() {
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
        self.product_event(&env, cx);
        match env.event {
            Event::Connected => {
                self.requests.connection_changed();
                #[cfg(debug_assertions)]
                if std::env::var_os("OCDESK_DEBUG_PANIC").is_some() {
                    panic!("OCDESK_DEBUG_PANIC: deliberate panic to test sidecar teardown");
                }
                self.store
                    .update(cx, |s, cx| s.set_connection(Connection::Connected, cx));
                self.resync(cx);
                // Watches may have fired while we were away.
                self.files_changed(None, None, false, cx);
            }
            Event::FileChanged { file, removed } => {
                self.files_changed(env.directory, Some(file), removed, cx)
            }
            Event::Disconnected => {
                self.requests.connection_changed();
                self.store.update(cx, |s, cx| {
                    s.cancel_history_loads();
                    s.abandon_replies(cx);
                    cx.notify();
                });
                self.store.update(cx, |s, cx| {
                    if s.connection() == Connection::Connected {
                        s.set_connection(Connection::Reconnecting, cx);
                    }
                });
            }
            Event::CatalogChanged | Event::ToolingChanged | Event::WorktreesChanged { .. } => {
                self.schedule_catalog_refresh(cx)
            }
            _ => {
                let turn_over = match &env.event {
                    Event::ExecutionEnded { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                };
                // A tool may have edited files: revalidate open tabs (cheap
                // 304s), whether or not a watch reports it. Known read-only
                // tools can't have; unknown ones (MCP) might.
                let touched = match &env.event {
                    Event::ToolSucceeded {
                        session_id,
                        message_id,
                        call_id,
                        ..
                    } => {
                        let name = self
                            .store
                            .read(cx)
                            .tool_name(session_id, message_id, call_id);
                        (!name.is_some_and(is_read_only_tool)).then(|| env.directory.clone())
                    }
                    _ => None,
                };
                self.store.update(cx, |s, cx| s.apply(env, cx));
                if let Some(dir) = touched {
                    self.files_changed(dir, None, false, cx);
                }
                if let Some(session_id) = turn_over {
                    self.on_turn_over(session_id, cx);
                }
            }
        }
    }

    /// A turn finished (`session.execution.succeeded|failed|interrupted`).
    fn on_turn_over(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.refresh_usage(session_id.clone(), cx);
        let reload = self
            .store
            .update(cx, |s, _| s.needs_reload.remove(&session_id));
        if reload
            && self
                .store
                .read(cx)
                .conversations
                .get(&session_id)
                .is_some_and(|c| c.loaded())
        {
            self.load_messages(session_id.clone(), cx);
        }
        // Events keep approvals current; re-read only what is still shown,
        // in case the turn ended without a replied/cancelled event.
        if self.store.read(cx).permissions.has_pending(&session_id) {
            self.refresh_permissions(session_id.clone(), cx);
        }
        if self.store.read(cx).forms.has_pending(&session_id) {
            self.refresh_forms(session_id.clone(), cx);
        }
        self.diff_maybe_changed(&session_id, cx);
        #[cfg(debug_assertions)]
        if std::env::var_os("OCDESK_VERIFY_ON_IDLE").is_some() {
            self.verify_against_rest(session_id, cx);
        }
        // Verification hook for dev/teardown-test.sh: quit through the normal
        // on_app_quit path once a turn finishes.
        if std::env::var_os("OCDESK_EXIT_ON_IDLE").is_some() {
            cx.quit();
        }
    }

    /// After every (re)connect frames may have been missed: re-fetch the
    /// session list, busy states, and the selected conversation.
    fn refresh_active(&mut self, cx: &mut Context<Self>) {
        self.read_active(0, cx);
    }

    fn read_active(&mut self, attempt: u32, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.read(cx).snapshot_revision;
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.active().await },
            move |this, res, cx| {
                if this.store.read(cx).snapshot_revision != revision
                    && this.retry_snapshot(attempt, cx, move |this, cx| {
                        this.read_active(attempt + 1, cx)
                    })
                {
                    return;
                }
                match res {
                    Ok(active) => this.store.update(cx, |s, cx| s.set_active(active, cx)),
                    Err(e) => tracing::warn!(op = "active", error = %format!("{e:#}"), "could not read active sessions"),
                }
            },
        );
    }

    fn resync(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        self.store
            .update(cx, |s, _| s.invalidate_except(selected.as_deref()));
        self.fetched_epoch.clear();
        if let Some(id) = &selected {
            self.fetched_epoch.insert(id.clone(), self.requests.epoch());
        }
        self.refresh_sessions(cx);
        self.refresh_active(cx);
        self.refresh_saved_grants(cx);
        // Approvals of every listed session (children included) are read per
        // folder by `refresh_sessions`.
        if let Some(id) = selected {
            self.refresh_session_mode(id.clone(), cx);
            self.refresh_usage(id.clone(), cx);
            self.refresh_inbox(id.clone(), cx);
            self.refresh_tasks(id.clone(), cx);
            self.load_messages(id.clone(), cx);
            if let Some(dir) = self.store.read(cx).selected_diff_dir()
                && self.store.read(cx).diffs.contains_key(&dir)
            {
                self.refresh_folder_diff(dir, cx);
            }
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
        let exec = cx.background_executor().clone();
        self.catalog_debounce = Some(cx.spawn(async move |this, cx| {
            exec.timer(CATALOG_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.refresh_catalogs(cx));
        }));
    }

    /// Re-read models, default model and agents for every folder.
    fn refresh_catalogs(&mut self, cx: &mut Context<Self>) {
        for folder in self.store.read(cx).folders.clone() {
            self.refresh_catalog(folder, cx);
        }
    }

    pub(crate) fn refresh_catalog(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        self.refresh_folder_info(folder.clone(), cx);
        let dir = folder.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (models, default_model, agents, configured_agent) = futures::join!(
                    api.models(&dir),
                    api.default_model(&dir),
                    api.agents(&dir),
                    api.configured_agent(&dir)
                );
                Ok(Catalog {
                    models: models?,
                    // No default configured is not an error.
                    default_model: default_model.unwrap_or_else(|e| {
                        tracing::warn!(op = "default_model", dir = %dir.display(), error = %format!("{e:#}"), "no default model");
                        None
                    }),
                    agents: agents?,
                    configured_agent: configured_agent.unwrap_or_default(),
                })
            },
            move |this, res: Result<Catalog>, cx| match res {
                Ok(c) => this.store.update(cx, |s, cx| s.set_catalog(folder, c, cx)),
                Err(e) => tracing::warn!(
                    op = "catalog",
                    dir = %folder.display(),
                    error = %format!("{e:#}"),
                    "could not read models/agents"
                ),
            },
        );
    }

    /// The folder's `/` menu sources, branch, project and worktrees. Each
    /// part fails on its own (e.g. a folder that isn't a git repository).
    fn refresh_folder_info(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let dir = folder.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (commands, skills, mcp, vcs, project) = futures::join!(
                    api.commands(&dir),
                    api.skills(&dir),
                    api.mcp_servers(&dir),
                    api.vcs(&dir),
                    api.project_id(&dir),
                );
                let worktrees = match &project {
                    Ok(pid) => api.worktrees(pid).await.ok(),
                    Err(_) => None,
                };
                // Branches of the worktrees too, for the sidebar.
                let mut wt_vcs = Vec::new();
                for w in worktrees.iter().flatten().filter(|w| **w != dir) {
                    if let Ok(v) = api.vcs(w).await {
                        wt_vcs.push((w.clone(), v));
                    }
                }
                let warn = |what: &str, e: &anyhow::Error| {
                    tracing::debug!(op = "folder_info", dir = %dir.display(), error = %format!("{e:#}"), "{what}")
                };
                let tooling = Tooling {
                    commands: commands.unwrap_or_else(|e| {
                        warn("no commands", &e);
                        vec![]
                    }),
                    skills: skills.unwrap_or_else(|e| {
                        warn("no skills", &e);
                        vec![]
                    }),
                    mcp: mcp.unwrap_or_else(|e| {
                        warn("no MCP servers", &e);
                        vec![]
                    }),
                };
                Ok((tooling, vcs.ok(), project.ok(), worktrees, wt_vcs))
            },
            move |this, res: Result<_>, cx| {
                let Ok((tooling, vcs, project, worktrees, wt_vcs)) = res else {
                    return;
                };
                this.store.update(cx, |s, cx| {
                    s.set_tooling(folder.clone(), tooling, cx);
                    if let Some(v) = vcs {
                        s.set_vcs(folder.clone(), v, cx);
                    }
                    for (w, v) in wt_vcs {
                        s.set_vcs(w, v, cx);
                    }
                    if let (Some(pid), Some(wts)) = (project, worktrees) {
                        s.set_worktrees(folder.clone(), pid, wts, cx);
                    }
                });
            },
        );
    }

    // ── git worktrees ───────────────────────────────────────────────────

    /// Create a worktree of `folder`'s project and open a new session in it.
    pub fn create_worktree(&mut self, folder: PathBuf, name: String, cx: &mut Context<Self>) {
        let Some(project) = self.store.read(cx).projects.get(&folder).cloned() else {
            self.notice(format!("{} is not a git repository", folder.display()), cx);
            return;
        };
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move {
                let dir = api.create_worktree(&project, name.trim()).await?;
                // Register the new checkout as a location before using it.
                api.location(&dir).await?;
                Ok(dir)
            },
            move |this, res, cx| match res {
                Ok(dir) => {
                    this.refresh_folder_info(folder, cx);
                    this.new_session(Some(dir), cx);
                }
                Err(e) => this.notice(format!("Could not create the worktree: {e:#}"), cx),
            },
        );
    }

    /// Remove a worktree. Without `force`, a dirty checkout is refused;
    /// `on_needs_force` is then called so the UI can ask.
    pub fn remove_worktree(
        &mut self,
        folder: PathBuf,
        dir: PathBuf,
        force: bool,
        on_needs_force: impl FnOnce(&mut gpui_kit::App) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.store.read(cx).projects.get(&folder).cloned() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        let target = dir.clone();
        self.run(
            cx,
            async move { api.remove_worktree(&project, &target, force).await },
            move |this, res, cx| match res {
                Ok(()) => {
                    if this.store.read(cx).target_folder().as_ref() == Some(&dir) {
                        this.new_session(Some(folder.clone()), cx);
                    }
                    this.refresh_folder_info(folder, cx);
                }
                Err(e) if !force && e.is::<crate::api::WorktreeDirty>() => on_needs_force(cx),
                Err(e) => this.notice(format!("Could not remove the worktree: {e:#}"), cx),
            },
        );
    }

    /// Re-pin the selected session to another checkout of its project.
    pub fn move_session(&mut self, sid: String, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let target = dir.clone();
        self.run(
            cx,
            async move {
                api.location(&target).await?;
                api.move_session(&sid, &target).await
            },
            move |this, res, cx| match res {
                // The server confirms with a session update; re-read the list.
                Ok(()) => {
                    this.store.update(cx, |s, cx| s.set_current_folder(dir, cx));
                    this.refresh_sessions(cx);
                }
                Err(e) => this.notice(format!("Could not move the session: {e:#}"), cx),
            },
        );
    }

    // ── background tasks: subagents and shells ──────────────────────────

    /// Re-read a session's subagent sessions and its folder's shells.
    pub fn refresh_tasks(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let Some(dir) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        let request = self.task_requests.bump(dir.clone());
        let revision = self.store.read(cx).tasks_revision;
        let target = sid.clone();
        let folder = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                let (kids, shells) = futures::join!(api.children(&sid), api.shells(&dir));
                Ok((kids?, shells?))
            },
            move |this, res, cx| {
                if !this.task_requests.is_current(&folder, request) {
                    return;
                }
                if this.store.read(cx).tasks_revision != revision {
                    this.refresh_tasks(target, cx);
                    return;
                }
                match res {
                    Ok((kids, shells)) => this.store.update(cx, |s, cx| {
                        s.set_children(&target, kids, cx);
                        s.set_shells(&folder, shells, cx);
                    }),
                    Err(e) => tracing::warn!(op = "tasks", session_id = %target, error = %format!("{e:#}"), "could not read tasks"),
                }
            },
        );
    }

    /// Load a subagent's transcript (it isn't in the sidebar, so nothing
    /// else would); live events keep it current afterwards.
    pub fn load_child(&mut self, child: String, cx: &mut Context<Self>) {
        let loaded = self
            .store
            .read(cx)
            .conversations
            .get(&child)
            .is_some_and(|c| c.loaded());
        if !loaded {
            self.load_messages(child, cx);
        }
    }

    /// The tail of a shell's output. `None` when opencode isn't running.
    pub fn shell_output(
        &self,
        dir: PathBuf,
        shell_id: String,
    ) -> Option<oneshot::Receiver<Result<String>>> {
        let api = self.api.clone()?;
        Some(reads::spawn_read(&self.rt, "shell_output", async move {
            api.shell_output(&dir, &shell_id).await
        }))
    }

    pub fn kill_shell(&mut self, dir: PathBuf, shell_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.kill_shell(&dir, &shell_id).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not stop the command: {e:#}"), cx);
                }
            },
        );
    }

    /// Compact the selected session's context (`/compact`).
    pub fn compact(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.notice("Open a session to compact it", cx);
            return;
        };
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.compact(&sid).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not compact: {e:#}"), cx);
                }
            },
        );
    }

    /// Branch the selected session at `before` into a new session and open it.
    pub fn fork(&mut self, before: Option<String>, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.fork_session(&sid, before.as_deref()).await },
            |this, res, cx| match res {
                Ok(session) => {
                    let id = session.id.clone();
                    this.store.update(cx, |s, cx| {
                        s.apply(Envelope::bare(Event::SessionUpserted(session)), cx)
                    });
                    this.select_session(Some(id), cx);
                }
                Err(e) => this.notice(format!("Could not fork: {e:#}"), cx),
            },
        );
    }

    /// Undo the selected session back to before `message_id` (its prompt,
    /// the replies and their file changes), then reload its history.
    pub fn undo_to(&mut self, message_id: String, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        if self.store.read(cx).is_busy(&sid) {
            self.notice("Stop the running turn before undoing", cx);
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let session = sid.clone();
        self.run(
            cx,
            async move { api.revert_to(&session, &message_id).await },
            move |this, res, cx| match res {
                Ok(()) => {
                    this.load_messages(sid.clone(), cx);
                    this.refresh_diff(sid, cx);
                }
                Err(e) => this.notice(format!("Could not undo: {e:#}"), cx),
            },
        );
    }

    /// Switch the selected session to the build agent and implement the
    /// plan it just wrote.
    pub fn implement_plan(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(sid) = store.selected.clone() else {
            return;
        };
        let Some(folder) = store.session(&sid).map(|s| s.directory.clone()) else {
            return;
        };
        let prompt = PromptInput {
            text: "Implement the plan above.".into(),
            ..Default::default()
        };
        if self.demo {
            self.send_prompt(prompt, cx);
            return;
        }
        let Some(api) = self.api(cx) else { return };
        // Shown at once and rolled back on failure (see domain::choice); the
        // draft default for new sessions only changes once the server agreed.
        let token = self.show_agent(&sid, "build", cx);
        let session = sid.clone();
        // Switching threads while the request runs must not retarget the
        // prompt. A sidecar restart also invalidates this continuation.
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.switch_agent(&session, "build").await },
            move |this, res, cx| {
                this.settle_agent(sid.clone(), token, "build".into(), res.is_err(), cx);
                match res {
                    Ok(()) => {
                        this.store.update(cx, |s, cx| {
                            s.set_draft_choice(None, Some("build".into()), cx)
                        });
                        this.submit_to(Submission::Prompt(prompt), Some((Some(sid), folder)), cx);
                    }
                    Err(e) => {
                        this.notice(format!("Could not switch to the build agent: {e:#}"), cx)
                    }
                }
            },
        );
    }

    /// Connect or disconnect an MCP server of the target folder.
    pub fn set_mcp_connected(&mut self, server: String, connect: bool, cx: &mut Context<Self>) {
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        let dir = folder.clone();
        self.run(
            cx,
            async move { api.mcp_set_connected(&dir, &server, connect).await },
            move |this, res, cx| match res {
                Ok(()) => this.refresh_folder_info(folder, cx),
                Err(e) => this.notice(format!("MCP: {e:#}"), cx),
            },
        );
    }

    /// Pick the model (and variant) for the selected session, or for the
    /// next new session when nothing is selected.
    pub fn choose_model(&mut self, model: ModelRef, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.store
                .update(cx, |s, cx| s.set_draft_choice(Some(model), None, cx));
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let token = self.show_model(&sid, &model, cx);
        let target = sid.clone();
        let chosen = model.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.switch_model(&sid, &model).await },
            move |this, res, cx| {
                this.settle_model(target, token, chosen, res.is_err(), cx);
                if let Err(e) = res {
                    this.notice(format!("Could not switch model: {e:#}"), cx);
                }
            },
        );
    }

    /// Pick the primary agent for the selected session, or the next new one.
    pub fn choose_agent(&mut self, agent: String, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.store
                .update(cx, |s, cx| s.set_draft_choice(None, Some(agent), cx));
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let token = self.show_agent(&sid, &agent, cx);
        let target = sid.clone();
        let chosen = agent.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.switch_agent(&sid, &agent).await },
            move |this, res, cx| {
                this.settle_agent(target, token, chosen, res.is_err(), cx);
                if let Err(e) = res {
                    this.notice(format!("Could not switch agent: {e:#}"), cx);
                }
            },
        );
    }

    // Optimistic choices: shown at once, confirmed by the stream
    // (session.model.selected / session.agent.selected). See domain::choice
    // for what a late or failed completion may still change.

    fn show_model(&mut self, sid: &str, model: &ModelRef, cx: &mut Context<Self>) -> ChoiceToken {
        let shown = self
            .store
            .read(cx)
            .session(sid)
            .and_then(|s| s.model.clone());
        let token = self.model_choices.choose(sid, shown);
        self.show_selection(
            Event::ModelSelected {
                session_id: sid.to_owned(),
                model: model.clone(),
            },
            cx,
        );
        token
    }

    fn settle_model(
        &mut self,
        sid: String,
        token: ChoiceToken,
        model: ModelRef,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if !failed {
            self.model_choices.succeeded(&sid, token, model);
        } else if let Some(model) = self.model_choices.failed(&sid, token) {
            tracing::debug!(session_id = %sid, "rolling back failed model choice");
            self.show_selection(
                Event::ModelSelected {
                    session_id: sid,
                    model,
                },
                cx,
            );
        }
    }

    fn show_agent(&mut self, sid: &str, agent: &str, cx: &mut Context<Self>) -> ChoiceToken {
        let shown = self
            .store
            .read(cx)
            .session(sid)
            .and_then(|s| s.agent.clone());
        let token = self.agent_choices.choose(sid, shown);
        self.show_selection(
            Event::AgentSelected {
                session_id: sid.to_owned(),
                agent: agent.to_owned(),
            },
            cx,
        );
        token
    }

    fn settle_agent(
        &mut self,
        sid: String,
        token: ChoiceToken,
        agent: String,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if !failed {
            self.agent_choices.succeeded(&sid, token, agent);
        } else if let Some(agent) = self.agent_choices.failed(&sid, token) {
            tracing::debug!(session_id = %sid, "rolling back failed agent choice");
            self.show_selection(
                Event::AgentSelected {
                    session_id: sid,
                    agent,
                },
                cx,
            );
        }
    }

    /// Apply a locally decided selection; it bypasses [`Self::on_event`], so
    /// the optimistic trackers don't mistake it for a server report.
    fn show_selection(&mut self, event: Event, cx: &mut Context<Self>) {
        self.store
            .update(cx, |s, cx| s.apply(Envelope::bare(event), cx));
    }

    /// Fuzzy file search for `@` mentions. `None` when opencode isn't running.
    pub fn find_files(
        &self,
        folder: PathBuf,
        query: String,
    ) -> Option<oneshot::Receiver<Result<Vec<FsEntry>>>> {
        match (self.api.clone(), self.demo) {
            (Some(api), _) => Some(reads::spawn_read(&self.rt, "find_files", async move {
                api.find_files(&folder, &query, 20).await
            })),
            // Blocking local searches are bounded and finish independently.
            (None, true) => {
                let (tx, rx) = oneshot::channel();
                self.rt.spawn_blocking(move || {
                    let _ = tx.send(Ok(files::find_local(&folder, &query, 20)));
                });
                Some(rx)
            }
            (None, false) => None,
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
            let Some(conv) = store.conversations.get(&session_id).filter(|c| c.loaded()) else { return };
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
        self.store.update(cx, |s, cx| {
            s.notice = Some(msg);
            cx.notify();
        });
        let exec = cx.background_executor().clone();
        let store = self.store.clone();
        self.notice_clear = Some(cx.spawn(async move |_, cx| {
            exec.timer(NOTICE_LINGER).await;
            store.update(cx, |s, cx| {
                if s.notice.take().is_some() {
                    cx.notify();
                }
            });
        }));
    }

    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        self.read_sessions(0, cx);
    }

    fn read_sessions(&mut self, attempt: u32, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.read(cx).snapshot_revision;
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.list_sessions().await },
            move |this, res, cx| {
                if this.store.read(cx).snapshot_revision != revision
                    && this.retry_snapshot(attempt, cx, move |this, cx| {
                        this.read_sessions(attempt + 1, cx)
                    })
                {
                    return;
                }
                match res {
                    Ok(list) => {
                        let mut by_folder: std::collections::HashMap<PathBuf, Vec<String>> =
                            Default::default();
                        for s in &list {
                            by_folder
                                .entry(s.directory.clone())
                                .or_default()
                                .push(s.id.clone());
                        }
                        // Forget unread marks of sessions that no longer exist.
                        let gone: Vec<String> = this
                            .settings
                            .unread
                            .iter()
                            .filter(|id| !list.iter().any(|s| &s.id == *id))
                            .cloned()
                            .collect();
                        if !gone.is_empty() {
                            this.update_settings(cx, |s| {
                                s.unread.retain(|id| !gone.contains(id));
                            });
                            this.store.update(cx, |s, _| {
                                s.unread.retain(|id| !gone.contains(id));
                            });
                        }
                        this.store.update(cx, |s, cx| s.set_sessions(list, cx));
                        // The list has no subagents, but the folder reads cover
                        // them: include the known ones so stale cards clear.
                        for child in this.store.read(cx).children.values().flatten() {
                            let ids = by_folder.entry(child.directory.clone()).or_default();
                            if !ids.contains(&child.id) {
                                ids.push(child.id.clone());
                            }
                        }
                        for (dir, ids) in by_folder {
                            this.refresh_folder_pending(dir, ids, cx);
                        }
                        this.restore_last_session(cx);
                    }
                    Err(e) => this.notice(format!("Could not list sessions: {e:#}"), cx),
                }
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
        // Only the latest fetch for a session (and for this sidecar) lands.
        let ticket = self.store.update(cx, |s, _| s.begin_load(&session_id));
        let sid = session_id.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.messages(&sid).await },
            move |this, res, cx| {
                if let Err(e) = &res {
                    tracing::warn!(%session_id, "could not load messages: {e:#}");
                }
                this.store
                    .update(cx, |s, cx| s.finish_load(&session_id, ticket, res, cx));
            },
        );
    }

    /// Pending permissions and question forms of every session in `dir`,
    /// in one request each (instead of two per session after every
    /// session-list refresh).
    fn refresh_folder_pending(
        &mut self,
        dir: PathBuf,
        sessions: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api.clone() else { return };
        // (session, permission revision, form revision) at request time.
        let revisions: Vec<(String, Revision, Revision)> = self.store.update(cx, |s, _| {
            sessions
                .into_iter()
                .map(|sid| {
                    let (p, f) = (s.permissions.revision(&sid), s.forms.revision(&sid));
                    (sid, p, f)
                })
                .collect()
        });
        let d = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                let read = |forms: bool| {
                    let (api, d) = (api.clone(), d.clone());
                    with_location_retry(move || {
                        let (api, d) = (api.clone(), d.clone());
                        async move {
                            Ok(if forms {
                                (Vec::new(), api.folder_forms(&d).await?)
                            } else {
                                (api.folder_permissions(&d).await?, Vec::new())
                            })
                        }
                    })
                };
                let (p, f) = futures::join!(read(false), read(true));
                anyhow::Ok((p.map(|x| x.0), f.map(|x| x.1)))
            },
            move |this, res, cx| {
                let Ok((permissions, forms)) = res else {
                    return;
                };
                let mut stale_permissions = Vec::new();
                let mut stale_forms = Vec::new();
                this.store.update(cx, |s, cx| {
                    for (sid, p_rev, f_rev) in &revisions {
                        // A live event (or newer snapshot) wins over this read.
                        if let Ok(all) = &permissions {
                            if !s.permissions.is_current(sid, *p_rev) {
                                stale_permissions.push(sid.clone());
                            } else {
                                let mine: Vec<_> = all
                                    .iter()
                                    .filter(|r| &r.session_id == sid)
                                    .cloned()
                                    .collect();
                                // Untouched sessions are skipped: no 200 notifies per refresh.
                                if !mine.is_empty() || s.permissions.has_pending(sid) {
                                    s.set_permissions(sid, mine, cx);
                                }
                            }
                        }
                        if let Ok(all) = &forms {
                            if !s.forms.is_current(sid, *f_rev) {
                                stale_forms.push(sid.clone());
                            } else {
                                let mine: Vec<_> = all
                                    .iter()
                                    .filter(|f| &f.session_id == sid)
                                    .cloned()
                                    .collect();
                                if !mine.is_empty() || s.forms.has_pending(sid) {
                                    s.set_forms(sid, mine, cx);
                                }
                            }
                        }
                    }
                });
                // A failed folder read falls back to per-session reads.
                for (sid, ..) in &revisions {
                    if permissions.as_ref().is_err_and(|e| !is_location_404(e)) {
                        stale_permissions.push(sid.clone());
                    }
                    if forms.as_ref().is_err_and(|e| !is_location_404(e)) {
                        stale_forms.push(sid.clone());
                    }
                }
                for e in [permissions.as_ref().err(), forms.as_ref().err()]
                    .into_iter()
                    .flatten()
                {
                    tracing::warn!(dir = %dir.display(), "folder approvals read failed: {e:#}");
                }
                for sid in stale_permissions {
                    this.refresh_permissions(sid, cx);
                }
                for sid in stale_forms {
                    this.refresh_forms(sid, cx);
                }
            },
        );
    }

    pub fn refresh_permissions(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.update(cx, |s, _| s.permissions.revision(&sid));
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    async move { api.permissions(&id).await }
                })
                .await
            },
            move |this, res, cx| {
                match res {
                    Ok(requests) => {
                        // A live asked/replied event (or newer snapshot) wins over this read.
                        if this.store.read(cx).permissions.is_current(&sid, revision) {
                            this.store
                                .update(cx, |s, cx| s.set_permissions(&sid, requests, cx));
                        } else {
                            this.refresh_permissions(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!(op = "permissions", session_id = %sid, error = %format!("{e:#}"), "location not ready; permission requests not loaded")
                    }
                    Err(e) => this.notice(format!("Could not load permission requests: {e:#}"), cx),
                }
            },
        );
    }

    pub fn refresh_forms(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.update(cx, |s, _| s.forms.revision(&sid));
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    async move { api.forms(&id).await }
                })
                .await
            },
            move |this, res, cx| {
                match res {
                    Ok(forms) => {
                        // A live created/replied event (or newer snapshot) wins over this read.
                        if this.store.read(cx).forms.is_current(&sid, revision) {
                            this.store.update(cx, |s, cx| s.set_forms(&sid, forms, cx));
                        } else {
                            this.refresh_forms(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!(op = "forms", session_id = %sid, error = %format!("{e:#}"), "location not ready; question forms not loaded")
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
        let (session, form) = (sid.clone(), fid.clone());
        let to = Reply {
            session_id: sid.clone(),
            request_id: fid.clone(),
            resolved: Event::FormReplied {
                session_id: sid,
                form_id: fid,
            },
            failure: "Could not reply",
        };
        self.send_reply(
            |s| &mut s.forms,
            to,
            async move { api.reply_form(&session, &form, &answer).await },
            |this, sid, _ok, cx| this.refresh_forms(sid, cx),
            cx,
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
        let (session, form) = (sid.clone(), fid.clone());
        let to = Reply {
            session_id: sid.clone(),
            request_id: fid.clone(),
            resolved: Event::FormCancelled {
                session_id: sid,
                form_id: fid,
            },
            failure: "Could not cancel",
        };
        self.send_reply(
            |s| &mut s.forms,
            to,
            async move { api.cancel_form(&session, &form).await },
            |this, sid, _ok, cx| this.refresh_forms(sid, cx),
            cx,
        );
    }

    pub fn reply_permission(
        &mut self,
        sid: String,
        rid: String,
        decision: PermissionDecision,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else { return };
        let (session, request) = (sid.clone(), rid.clone());
        let to = Reply {
            session_id: sid.clone(),
            request_id: rid.clone(),
            resolved: Event::PermissionReplied {
                session_id: sid,
                request_id: rid,
            },
            failure: "Could not reply",
        };
        self.send_reply(
            |s| &mut s.permissions,
            to,
            async move {
                api.reply_permission(&session, &request, decision, message.as_deref())
                    .await
            },
            move |this, sid, ok, cx| {
                this.refresh_permissions(sid, cx);
                if ok && decision == PermissionDecision::Always {
                    this.refresh_saved_grants(cx);
                }
            },
            cx,
        );
    }

    /// Answer a pending permission request or question form (see
    /// `domain::approvals`). Refused while disconnected or while a reply to
    /// the same request is in flight. Success applies `resolved` at once (the
    /// stream confirms it too); failure keeps the request visible with the
    /// error, retryable. Either way the session's requests are re-read by
    /// `then`, in case the server's view differs.
    ///
    /// Connection-scoped: a disconnect abandons the reply's "sending" state
    /// (`Store::abandon_replies`), so its completion must not land later.
    fn send_reply<R: PendingRequest + 'static>(
        &mut self,
        ledger: fn(&mut Store) -> &mut PendingRequests<R>,
        to: Reply,
        reply: impl Future<Output = Result<()>> + Send + 'static,
        then: impl FnOnce(&mut Self, String, bool, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Reply {
            session_id: sid,
            request_id: rid,
            resolved,
            failure,
        } = to;
        if self.store.read(cx).connection() != Connection::Connected {
            return;
        }
        let started = self.store.update(cx, |s, cx| {
            let started = ledger(s).begin_reply(&sid, &rid);
            if started {
                cx.notify();
            }
            started
        });
        if !started {
            return;
        }
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            reply,
            move |this, res, cx| {
                let ok = res.is_ok();
                this.store.update(cx, |s, cx| match res {
                    Ok(()) => s.apply(Envelope::bare(resolved), cx),
                    Err(e) => {
                        tracing::warn!(op = failure, session_id = %sid, request_id = %rid, error = %format!("{e:#}"), "reply failed");
                        ledger(s).reply_failed(&sid, &rid, format!("{failure}: {e:#}"));
                        cx.notify();
                    }
                });
                then(this, sid, ok, cx);
            },
        );
    }

    /// Reload the working-tree diff of `sid`'s folder (shared by all of
    /// its sessions).
    pub fn refresh_diff(&mut self, sid: String, cx: &mut Context<Self>) {
        // An unlisted (subagent) session has no folder entry: nothing to load,
        // and no spinner may be left on.
        let Some(dir) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        self.refresh_folder_diff(dir, cx);
    }

    fn refresh_folder_diff(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let request = self.diff_requests.bump(dir.clone());
        self.store.update(cx, |s, cx| s.begin_diff(dir.clone(), cx));
        let d = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.working_diff(&d).await },
            move |this, res, cx| {
                if !this.diff_requests.is_current(&dir, request) {
                    return;
                }
                if let Ok(files) = &res {
                    this.prune_viewed(&dir, files, cx);
                }
                this.store.update(cx, |s, cx| match res {
                    Ok(files) => s.set_diff_state(dir, Some(files), None, cx),
                    Err(e) => s.set_diff_state(
                        dir,
                        None,
                        Some(format!("Could not load changes: {e:#}")),
                        cx,
                    ),
                });
            },
        );
    }

    /// A turn may have changed files in `dir`: reload its diff if a view
    /// shows it (the selected session's folder), else mark it stale so it
    /// reloads when next selected. Unseen folders aren't fetched at all.
    fn diff_maybe_changed(&mut self, sid: &str, cx: &mut Context<Self>) {
        let s = self.store.read(cx);
        let Some(dir) = s.session(sid).map(|x| x.directory.clone()) else {
            return;
        };
        if !s.diffs.contains_key(&dir) {
            return;
        }
        if s.selected_diff_dir().as_ref() == Some(&dir) {
            self.refresh_folder_diff(dir, cx);
        } else {
            self.store.update(cx, |s, _| {
                s.diff_stale.insert(dir);
            });
        }
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
        self.mark_selected_read(cx);
        if let Some(id) = id {
            // Cheap, and no event reports a change made elsewhere.
            self.refresh_session_mode(id.clone(), cx);
            self.refresh_usage(id.clone(), cx);
            if let Some(dir) = self.store.read(cx).selected_diff_dir()
                && self.store.read(cx).diff_stale.contains(&dir)
            {
                self.refresh_folder_diff(dir, cx);
            }
            // A loaded transcript, its approvals, inbox and tasks are kept
            // current by events, so switching back to a session seen on this
            // connection needs no refetch (a reconnect bumps the epoch, and
            // `invalidate_except` unloads transcripts).
            let current = {
                let s = self.store.read(cx);
                s.conversations
                    .get(&id)
                    .is_some_and(|c| c.loaded() || c.fetching())
                    && !s.needs_reload.contains(&id)
                    && self.fetched_epoch.get(&id) == Some(&self.requests.epoch())
            };
            if current {
                return;
            }
            self.fetched_epoch.insert(id.clone(), self.requests.epoch());
            self.refresh_permissions(id.clone(), cx);
            self.refresh_forms(id.clone(), cx);
            self.refresh_inbox(id.clone(), cx);
            self.refresh_tasks(id.clone(), cx);
            self.load_messages(id, cx);
        }
    }

    /// Codex-style: "New session" clears the selection and picks the folder;
    /// the session is created on the first prompt so empty sessions don't
    /// pile up.
    pub fn new_session(&mut self, folder: Option<PathBuf>, cx: &mut Context<Self>) {
        let folder = folder.or_else(|| self.store.read(cx).target_folder());
        self.select_session(None, cx);
        self.reset_draft_choices(cx);
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
        let forgotten = self.store.update(cx, |s, cx| {
            let forgotten = s.forget_folder(&folder);
            s.set_folders(folders, cx);
            forgotten
        });
        // Worktree checkouts go with their folder: an in-flight diff or task
        // read for one must not re-create state for a directory now gone.
        for dir in &forgotten {
            self.diff_requests.forget(dir);
            self.task_requests.forget(dir);
        }
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
        if input.text.trim().is_empty() && input.attachments.is_empty() && input.files.is_empty() {
            return;
        }
        self.submit(Submission::Prompt(input), cx);
    }

    /// Run a server command (`/name args`) in the selected session, or in a
    /// new one.
    pub fn run_command(
        &mut self,
        name: String,
        text: String,
        delivery: Option<crate::api::model::Delivery>,
        cx: &mut Context<Self>,
    ) {
        self.submit(
            Submission::Command {
                name,
                text,
                delivery,
            },
            cx,
        );
    }

    pub fn retry_failed_submission(&mut self, cx: &mut Context<Self>) {
        if let Some((what, sid, folder)) = self.failed_submission.take() {
            self.submit_to(what, Some((sid, folder)), cx);
        }
    }

    pub fn has_failed_submission(&self) -> bool {
        self.failed_submission.is_some()
    }

    fn submit(&mut self, what: Submission, cx: &mut Context<Self>) {
        self.submit_to(what, None, cx);
    }

    fn submit_to(
        &mut self,
        what: Submission,
        target: Option<(Option<String>, PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        if self.demo {
            let text = match what {
                Submission::Prompt(i) => i.text,
                Submission::Command { name, text, .. } => format!("/{name} {text}"),
            };
            crate::demo::stream_reply(self.store.clone(), text, cx);
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let store = self.store.read(cx);
        let selected = target
            .as_ref()
            .map(|t| t.0.clone())
            .unwrap_or_else(|| store.selected.clone());
        let original_selection = store.selected.clone();
        let (draft_model, draft_agent) = (store.draft_model.clone(), store.draft_agent.clone());
        let Some(folder) = target.map(|t| t.1).or_else(|| store.target_folder()) else {
            self.notice("Add a project first", cx);
            return;
        };
        let permission_mode = store.draft_permission;
        let recovery = (what.clone(), selected.clone(), folder.clone());
        self.submission_seq += 1;
        let submission = self.submission_seq;
        let submission_target = selected
            .clone()
            .map(SubmissionTarget::Session)
            .unwrap_or_else(|| SubmissionTarget::Draft(folder.clone()));
        self.store.update(cx, |s, cx| {
            s.submitting.insert(submission, submission_target.clone());
            if let SubmissionTarget::Session(sid) = &submission_target {
                cx.emit(crate::store::StoreEvent::StatusChanged {
                    session_id: sid.clone(),
                });
            }
            cx.notify();
        });
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (sid, created) = match selected {
                    Some(sid) => (sid, None),
                    None => {
                        let mut s = api.create_session(&folder).await?;
                        if let Some(mode) = permission_mode {
                            api.set_session_mode(&s.id, mode).await?;
                        }
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
                match &what {
                    Submission::Prompt(input) => api.prompt(&sid, input).await?,
                    Submission::Command {
                        name,
                        text,
                        delivery,
                    } => api.run_command(&sid, name, text, *delivery).await?,
                }
                Ok((sid, created))
            },
            move |this, res, cx| {
                this.store.update(cx, |s, cx| {
                    s.submitting.remove(&submission);
                    if let SubmissionTarget::Session(sid) = &submission_target {
                        cx.emit(crate::store::StoreEvent::StatusChanged { session_id: sid.clone() });
                    }
                    cx.notify();
                });
                match res {
                Ok((sid, Some(session))) => {
                    this.store.update(cx, |s, cx| {
                        s.apply(Envelope::bare(Event::SessionUpserted(session)), cx);
                    });
                    if this.store.read(cx).selected == original_selection {
                        this.select_session(Some(sid), cx);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    this.failed_submission = Some(recovery);
                    this.notice(format!("Could not confirm prompt admission: {e:#}. Check history/inbox before retrying; the server may have accepted it."), cx);
                }
                }
            },
        );
    }

    // ── inbox: queued and steering prompts ──────────────────────────────

    /// Re-read a session's pending inbox (select, reconnect).
    pub fn refresh_inbox(&mut self, sid: String, cx: &mut Context<Self>) {
        self.read_inbox(sid, 0, cx);
    }

    fn read_inbox(&mut self, sid: String, attempt: u32, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let target = sid.clone();
        let revision = self
            .store
            .update(cx, |s, _| s.inbox_revision.current(sid.clone()));
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.inbox(&sid).await },
            move |this, res, cx| {
                if !this.store.read(cx).inbox_revision.is_current(&target, revision) {
                    let again = target.clone();
                    if this.retry_snapshot(attempt, cx, move |this, cx| {
                        this.read_inbox(again, attempt + 1, cx)
                    }) {
                        return;
                    }
                }
                match res {
                    Ok(items) => this
                        .store
                        .update(cx, |s, cx| s.set_inbox(&target, items, cx)),
                    Err(e) => tracing::warn!(op = "inbox", session_id = %target, error = %format!("{e:#}"), "could not read inbox"),
                }
            },
        );
    }

    /// Withdraw a queued/steering prompt that hasn't been delivered yet.
    pub fn cancel_inbox(&mut self, sid: String, item_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.cancel_inbox(&sid, &item_id).await },
            |this, res, cx| {
                if let Err(e) = res {
                    // Most likely delivered in the meantime.
                    this.notice(format!("Could not remove the queued prompt: {e:#}"), cx);
                }
            },
        );
    }

    /// Turn a queued prompt into a steer (delivered at the next step).
    pub fn steer_now(&mut self, sid: String, item_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move {
                api.set_inbox_delivery(&sid, &item_id, crate::api::model::Delivery::Steer)
                    .await
            },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not steer: {e:#}"), cx);
                }
            },
        );
    }

    pub fn abort(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        if self.store.read(cx).interrupting.contains(&sid) {
            return;
        }
        let session = sid.clone();
        self.store.update(cx, |s, cx| {
            s.interrupting.insert(sid.clone());
            cx.emit(crate::store::StoreEvent::StatusChanged {
                session_id: sid.clone(),
            });
            cx.notify();
        });
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.interrupt(&sid).await },
            move |this, res, cx| {
                if let Err(e) = res {
                    this.store.update(cx, |s, cx| {
                        s.interrupting.remove(&session);
                        cx.emit(crate::store::StoreEvent::StatusChanged {
                            session_id: session,
                        });
                        cx.notify();
                    });
                    this.notice(format!("Could not stop: {e:#}"), cx);
                } else {
                    this.refresh_active(cx);
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

/// A reply to one pending request, for [`Controller::send_reply`].
struct Reply {
    session_id: String,
    request_id: String,
    /// Applied locally once the server accepted the reply.
    resolved: Event,
    /// Prefix of the error shown on the request when the reply fails.
    failure: &'static str,
}

/// What the composer sends: a prompt or a server command.
#[derive(Clone)]
enum Submission {
    Prompt(PromptInput),
    Command {
        name: String,
        text: String,
        delivery: Option<crate::api::model::Delivery>,
    },
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
