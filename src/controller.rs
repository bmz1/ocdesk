//! Turns user intents into API calls and API results into store updates.
//!
//! Owns the sidecar lifecycle (start, crash detection, auto-restart), the
//! SSE pump, and the folder list. One sidecar serves every folder:
//! sessions are pinned to a location (directory) and `/api/event` covers all
//! locations. All network work runs on the Tokio runtime; results come back
//! to the GPUI thread through channels awaited in `cx.spawn` tasks.

mod features;

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
use crate::store::{Catalog, Connection, Store, Tooling};

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
    grant_requests: u64,
    mode_requests: std::collections::HashMap<String, u64>,
    usage_revision: std::collections::HashMap<String, u64>,
    attention_seen: std::collections::HashSet<String>,
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
    failed_submission: Option<(Submission, Option<String>, PathBuf)>,
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
        let (draft_model, draft_agent) =
            (settings.draft_model.clone(), settings.draft_agent.clone());
        store.update(cx, |s, cx| {
            s.preferences = settings.clone();
            s.unread = settings.unread.clone();
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
            mode_requests: Default::default(),
            grant_requests: 0,
            usage_revision: Default::default(),
            attention_seen: Default::default(),
            pump: None,
            exit_watch: None,
            recent_crashes: Vec::new(),
            notice_seq: 0,
            catalog_seq: 0,
            demo: false,
            failed_submission: None,
            restore_session,
            save_seq: 0,
        };
        let folder = this.store.read(cx).target_folder();
        this.apply_draft_defaults(folder.as_deref(), cx);
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
        // The newest snapshot: it waits for any write in flight, then wins.
        if let Err(e) = self.settings.snapshot_for_save().write() {
            tracing::warn!("could not save settings: {e:#}");
        }
    }

    fn save_settings(&self) {
        // Numbered here, on the UI thread, so request order decides which
        // write sticks however the background writes interleave.
        let pending = self.settings.snapshot_for_save();
        self.rt.spawn_blocking(move || {
            if let Err(e) = pending.write() {
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
            s.mode_sending.clear();
            s.usage_loading.clear();
            s.grant_sending.clear();
            s.saved_grants_loading = false;
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
        let (tx, mut rx) = mpsc::channel(32);
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
        self.product_event(&env, cx);
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
            Event::CatalogChanged | Event::ToolingChanged | Event::WorktreesChanged { .. } => {
                self.schedule_catalog_refresh(cx)
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
        self.refresh_usage(_session_id.clone(), cx);
        let reload = self
            .store
            .update(cx, |s, _| s.needs_reload.remove(&_session_id));
        if reload
            && self
                .store
                .read(cx)
                .conversations
                .get(&_session_id)
                .is_some_and(|c| c.loaded)
        {
            self.load_messages(_session_id.clone(), cx);
        }
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
    fn refresh_active(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let guard = (self.generation, self.connection_epoch, self.store.read(cx).snapshot_revision);
        self.run(cx, async move { api.active().await }, move |this, res, cx| {
            if (this.generation, this.connection_epoch) != (guard.0, guard.1) { return; }
            if this.store.read(cx).snapshot_revision != guard.2 {
                this.refresh_active(cx);
                return;
            }
            match res {
                Ok(active) => this.store.update(cx, |s, cx| s.set_active(active, cx)),
                Err(e) => tracing::warn!("could not read active sessions: {e:#}"),
            }
        });
    }

    fn resync(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        self.store
            .update(cx, |s, _| s.invalidate_except(selected.as_deref()));
        self.refresh_sessions(cx);
        self.refresh_active(cx);
        let mut pending_sessions: std::collections::HashSet<String> =
            self.store.read(cx).permissions.keys().cloned().collect();
        pending_sessions.extend(self.store.read(cx).forms.keys().cloned());
        if let Some(id) = selected {
            self.refresh_session_mode(id.clone(), cx);
            self.refresh_usage(id.clone(), cx);
            pending_sessions.insert(id.clone());
            self.refresh_inbox(id.clone(), cx);
            self.refresh_tasks(id.clone(), cx);
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
        self.refresh_folder_info(folder.clone(), cx);
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

    /// The folder's `/` menu sources, branch, project and worktrees. Each
    /// part fails on its own (e.g. a folder that isn't a git repository).
    fn refresh_folder_info(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let generation = self.generation;
        let dir = folder.clone();
        self.run(
            cx,
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
                    tracing::debug!("{what} for {}: {e:#}", dir.display())
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
                if this.generation != generation {
                    return;
                }
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
        let dir = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone());
        let target = sid.clone();
        self.run(
            cx,
            async move {
                let kids = api.children(&sid).await?;
                let shells = match dir {
                    Some(d) => api.shells(&d).await.unwrap_or_default(),
                    None => vec![],
                };
                Ok((kids, shells))
            },
            move |this, res, cx| match res {
                Ok((kids, shells)) => this.store.update(cx, |s, cx| {
                    s.set_children(&target, kids, cx);
                    s.set_shells(shells, cx);
                }),
                Err(e) => tracing::warn!("could not read tasks of {target}: {e:#}"),
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
            .is_some_and(|c| c.loaded);
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
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(api.shell_output(&dir, &shell_id).await);
        });
        Some(rx)
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
        let Some(sid) = self.store.read(cx).selected.clone() else {
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
        self.store.update(cx, |s, cx| {
            s.set_draft_choice(None, Some("build".into()), cx);
            s.apply(
                Envelope::bare(Event::AgentSelected {
                    session_id: sid.clone(),
                    agent: "build".into(),
                }),
                cx,
            )
        });
        // The prompt must run with the new agent, so it waits for the switch.
        self.run(
            cx,
            async move { api.switch_agent(&sid, "build").await },
            move |this, res, cx| match res {
                Ok(()) => this.send_prompt(prompt, cx),
                Err(e) => this.notice(format!("Could not switch to the build agent: {e:#}"), cx),
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
        let guard = (self.generation, self.connection_epoch, self.store.read(cx).snapshot_revision);
        self.run(
            cx,
            async move { api.list_sessions().await },
            move |this, res, cx| {
                if (this.generation, this.connection_epoch) != (guard.0, guard.1) { return; }
                if this.store.read(cx).snapshot_revision != guard.2 {
                    this.refresh_sessions(cx);
                    return;
                }
                match res {
                Ok(list) => {
                    let ids: Vec<_> = list.iter().map(|s| s.id.clone()).collect();
                    this.store.update(cx, |s, cx| s.set_sessions(list, cx));
                    for id in ids {
                        this.refresh_permissions(id, cx);
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
        let generation = self.generation;
        let epoch = self.connection_epoch;
        let sid = session_id.clone();
        self.run(
            cx,
            async move { api.messages(&sid).await },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                if this.connection_epoch != epoch { return; }
                if let Err(e) = &res {
                    tracing::warn!(%session_id, "could not load messages: {e:#}");
                }
                this.store
                    .update(cx, |s, cx| s.finish_load(&session_id, ticket, res, cx));
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
        message: Option<String>,
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
            async move {
                api.reply_permission(&session, &request, decision, message.as_deref())
                    .await
            },
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
        if self.demo {
            return;
        }
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
        let mode = self
            .store
            .read(cx)
            .review
            .get(&sid)
            .cloned()
            .unwrap_or_default();
        let base = self.store.read(cx).review_base.get(&sid).cloned();
        let Some(dir) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        self.run(
            cx,
            async move { api.review_diff(&id, &dir, &mode, base.as_deref()).await },
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
        self.mark_selected_read(cx);
        if let Some(id) = id {
            self.refresh_session_mode(id.clone(), cx);
            self.refresh_usage(id.clone(), cx);
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
        self.apply_draft_defaults(folder.as_deref(), cx);
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

    pub fn has_failed_submission(&self) -> bool { self.failed_submission.is_some() }

    fn submit(&mut self, what: Submission, cx: &mut Context<Self>) {
        self.submit_to(what, None, cx);
    }

    fn submit_to(&mut self, what: Submission, target: Option<(Option<String>, PathBuf)>, cx: &mut Context<Self>) {
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
        let selected = target.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| store.selected.clone());
        let original_selection = store.selected.clone();
        let (draft_model, draft_agent) = (store.draft_model.clone(), store.draft_agent.clone());
        let Some(folder) = target.map(|t| t.1).or_else(|| store.target_folder()) else {
            self.notice("Add a folder first", cx);
            return;
        };
        let permission_mode = store
            .draft_permission
            .unwrap_or_else(|| self.settings.permission_for(&folder));
        let recovery = (what.clone(), selected.clone(), folder.clone());
        let generation = self.generation;
        self.run(
            cx,
            async move {
                let (sid, created) = match selected {
                    Some(sid) => (sid, None),
                    None => {
                        let mut s = api.create_session(&folder).await?;
                        api.set_session_mode(&s.id, permission_mode).await?;
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
                if this.generation != generation { return; }
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
        let Some(api) = self.api.clone() else { return };
        let target = sid.clone();
        let guard = (self.generation, self.connection_epoch, self.store.read(cx).snapshot_revision);
        self.run(
            cx,
            async move { api.inbox(&sid).await },
            move |this, res, cx| {
                if (this.generation, this.connection_epoch) != (guard.0, guard.1) { return; }
                if this.store.read(cx).snapshot_revision != guard.2 {
                    this.refresh_inbox(target, cx);
                    return;
                }
                match res {
                Ok(items) => this
                    .store
                    .update(cx, |s, cx| s.set_inbox(&target, items, cx)),
                Err(e) => tracing::warn!("could not read inbox of {target}: {e:#}"),
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

