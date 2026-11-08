//! Turns user intents into API calls and API results into store updates.
//!
//! Owns the sidecar lifecycle (start, crash detection, auto-restart), the
//! SSE pump, and the folder list. One sidecar serves every folder:
//! sessions are pinned to a location (directory) and `/api/event` covers all
//! locations. All network work runs on the Tokio runtime; results come back
//! to the GPUI thread through channels awaited in `cx.spawn` tasks.

mod approvals;
mod catalog;
mod choices;
mod diffs;
mod features;
mod files;
mod folders;
mod github;
mod lifecycle;
mod persistence;
mod prompts;
mod reads;
mod sessions;
mod tasks;

use prompts::Submission;

use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui_kit::{Context, Entity, Task};

use crate::api::model::{FsEntry, ModelRef, PermissionDecision, PromptInput};
use crate::api::{Api, Envelope, Event, HttpError};
use crate::domain::choice::{ChoiceToken, OptimisticChoices};
use crate::domain::requests::{
    ConnectionEpoch, Freshness, Issued, RequestClock, RequestScope, Revision, Revisions,
    ServerGeneration,
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

    // Optimistic choices: shown at once, confirmed by the stream
    // (session.model.selected / session.agent.selected). See domain::choice
    // for what a late or failed completion may still change.

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
