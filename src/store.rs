//! App state: the single source of truth for folders, sessions and//! conversations.
//!
//! The controller folds v2 events into it with [`Store::apply`] and replaces
//! a conversation with the REST projection via [`Store::reconcile`]. Views
//! observe `Entity<Store>` and subscribe to [`StoreEvent`] to re-measure only
//! what changed. Views never mutate it directly; they go through the
//! controller.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui_kit::{Context, EventEmitter};

use crate::api::events::Outcome;
use crate::api::model::{
    AgentInfo, AssistantMessage, Block, BlockKind, CommandInfo, FileDiff, FormRequest, InboxItem,
    McpServer, Message, Meta, ModelInfo, ModelRef, PermissionRequest, SessionInfo, ShellInfo,
    SkillInfo, Timing, ToolCall, ToolStatus, UserMessage, VcsInfo,
};
use crate::api::{Envelope, Event};
use crate::domain::fetch::{FetchStatus, FetchStatuses};
use crate::domain::history::{HistoryLoad, HistoryRequests, LoadTicket};

mod events;
mod forms;
mod history;
mod permissions;
mod sessions;
mod tasks;
mod workspace;

use events::{append_overlapping, prose_mut};

use crate::domain::requests::Revisions;
pub(crate) use forms::Forms;
pub(crate) use permissions::Permissions;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Connection {
    /// Sidecar is starting (first launch or restart).
    Starting,
    Connecting,
    Connected,
    /// SSE dropped; the pump is retrying.
    Reconnecting,
    /// The sidecar process is gone.
    SidecarDown {
        message: String,
        last_lines: Vec<String>,
    },
}

#[derive(Default)]
pub struct Conversation {
    /// Sorted by message id (ids are time-ordered).
    pub messages: Vec<Message>,
    /// Availability and ownership of the latest REST history request.
    load: HistoryLoad,
    /// Highest durable sequence number folded in.
    pub last_seq: Option<u64>,
    /// Messages that events touched while the fetch was in flight: the only
    /// local messages missing from the snapshot that survive it (others were
    /// reverted, compacted away or deleted on the server).
    pub live: HashSet<String>,
    /// Events for a not-yet-loaded conversation, replayed after its snapshot.
    pub pending: Vec<Envelope>,
    /// Prose blocks (message, kind, ordinal) whose start streamed before
    /// this conversation was loaded. opencode's history has no text for a
    /// block still streaming and deltas carry no offset, so only the tail is
    /// known until `BlockEnded` brings the full text.
    joined_mid_stream: HashSet<(String, BlockKind, u64)>,
    /// Bumped whenever `messages` change other than by streamed text, so
    /// views can memoize what they derive from tool calls and message
    /// structure (todos) instead of rescanning every frame.
    pub revision: u64,
}

/// Transcripts kept in memory besides live ones (recently selected sessions).
const KEEP_CONVERSATIONS: usize = 10;

/// Events buffered per loading conversation, at most.
const MAX_PENDING: usize = 128;

impl Conversation {
    pub(crate) fn loaded(&self) -> bool {
        self.load.loaded()
    }

    pub(crate) fn fetching(&self) -> bool {
        self.load.fetching()
    }

    /// Whether this prose block's beginning was missed (see
    /// `joined_mid_stream`): its text is only a tail, not renderable markdown.
    pub fn joined_mid_stream(&self, message_id: &str, kind: BlockKind, ordinal: u64) -> bool {
        self.joined_mid_stream
            .contains(&(message_id.to_string(), kind, ordinal))
    }

    fn cancel_load(&mut self) {
        self.load.cancel();
        self.live.clear();
        self.pending.clear();
    }

    /// `Ok(index)` of message `id`, else `Err(len)`: new messages append.
    ///
    /// The transcript is in *delivery* order, not id order (a queued prompt
    /// is created before a later steer but delivered after it), so this is
    /// a search from the end, where streaming happens, not a binary search.
    pub fn index_of(&self, id: &str) -> Result<usize, usize> {
        self.messages
            .iter()
            .rposition(|m| m.id() == id)
            .ok_or(self.messages.len())
    }

    /// The assistant message `id`, created on demand (a block can arrive
    /// before its step.started if we connected mid-turn).
    fn assistant_mut(&mut self, id: &str) -> (usize, bool, &mut AssistantMessage) {
        let (index, inserted) = match self.index_of(id) {
            Ok(i) => (i, false),
            Err(i) => {
                let msg = AssistantMessage {
                    id: id.to_string(),
                    ..Default::default()
                };
                self.messages.insert(i, Message::Assistant(msg));
                (i, true)
            }
        };
        match &mut self.messages[index] {
            Message::Assistant(a) => (index, inserted, a),
            // `apply_inner` drops assistant events for non-assistant ids.
            _ => unreachable!("message id {id} is not an assistant message"),
        }
    }
}

/// What changed, so views can do the minimum amount of relayout.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreEvent {
    FoldersChanged,
    SessionsChanged,
    SelectionChanged,
    /// The selected conversation's rows were replaced wholesale.
    MessagesReset {
        session_id: String,
    },
    MessageInserted {
        session_id: String,
        index: usize,
    },
    /// Content of one row changed (a block was added, grew, or finished).
    MessageChanged {
        session_id: String,
        index: usize,
    },
    StatusChanged {
        session_id: String,
    },
    ConnectionChanged,
    /// A folder's models/agents, or the composer's model/agent choice, changed.
    ChoicesChanged,
    /// A session's pending inbox (queued/steering prompts) changed.
    InboxChanged {
        session_id: String,
    },
    /// Subagent sessions or background shells changed.
    TasksChanged,
    /// Branches, worktrees or `/` menu sources of a folder changed.
    FolderInfoChanged,
    /// Files in a folder may have changed on disk: open editor tabs should
    /// revalidate. `dir: None` means any folder, `file: None` any file.
    FilesChanged {
        dir: Option<std::path::PathBuf>,
        file: Option<String>,
        removed: bool,
    },
    /// A session's diff snapshot or loader/error state changed.
    DiffsChanged {
        dir: PathBuf,
    },
    /// The set of files the user marked reviewed changed.
    ViewedChanged,
    /// A checkout's pull request, its fetch state or a check log changed.
    PullRequestChanged,
    /// A session's usage report, or its loading/error state, changed.
    UsageChanged {
        session_id: String,
    },
}

/// The user's reply to a pending permission request or question form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplyState {
    /// In flight; a second reply is refused.
    Sending,
    /// Failed; the request stays visible and can be answered again.
    Failed(String),
}

/// Result of [`Store::apply_m7`].
enum M7 {
    Handled(Vec<StoreEvent>),
    Continue(Vec<StoreEvent>),
}

/// What `/` offers in a folder: server commands, skills and MCP servers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tooling {
    pub commands: Vec<CommandInfo>,
    pub skills: Vec<SkillInfo>,
    pub mcp: Vec<McpServer>,
}

/// What a folder offers: its models, default model and agents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    pub models: Vec<ModelInfo>,
    pub default_model: Option<ModelRef>,
    pub configured_agent: Option<String>,
    pub agents: Vec<AgentInfo>,
}

impl Catalog {
    pub fn model(&self, r: &ModelRef) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|m| m.provider_id == r.provider_id && m.id == r.id)
    }

    /// The agent a session without an explicit choice runs: `build` if it
    /// exists, else the first primary agent.
    pub fn default_agent(&self) -> Option<&AgentInfo> {
        let primary = || self.agents.iter().filter(|a| a.is_primary());
        if let Some(agent) = self.configured_agent.as_deref()
            && let Some(agent) = primary().find(|a| a.id == agent)
        {
            return Some(agent);
        }
        primary()
            .find(|a| a.id == "build")
            .or_else(|| primary().next())
    }
}

/// What we know about a checkout's pull request.
#[derive(Default)]
pub struct PrEntry {
    /// `None` until the first fetch, then `Some(None)` when there's no PR.
    pub pr: Option<Option<crate::github::PullRequest>>,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<std::time::Instant>,
}

/// A failed check's log excerpt, by check link.
#[derive(Clone)]
pub enum CheckLog {
    Loading,
    Ready(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmissionTarget {
    Session(String),
    Draft(PathBuf),
}

#[derive(Default)]
pub struct Store {
    history_requests: HistoryRequests,
    /// Local HTTP admissions, visible before the server emits execution events.
    pub submitting: HashMap<u64, SubmissionTarget>,
    pub interrupting: HashSet<String>,
    pub snapshot_revision: u64,
    /// Per-session inbox events, so a read of one inbox isn't invalidated
    /// by activity in every other session.
    pub(crate) inbox_revision: Revisions<String>,
    pub tasks_revision: u64,
    pub(crate) shell_snapshots: HashMap<PathBuf, HashSet<String>>,
    /// The `gh` CLI is installed: the Pull request tab and chip show.
    pub gh_available: bool,
    /// Per checkout directory.
    pub pull_requests: HashMap<PathBuf, PrEntry>,
    pub check_logs: HashMap<String, CheckLog>,
    pub saved_grants: Vec<crate::api::features::SavedGrant>,
    pub(crate) saved_grants_status: FetchStatus,
    pub grant_sending: HashSet<String>,
    pub preferences: crate::settings::Settings,
    pub unread: std::collections::BTreeSet<String>,
    pub window_active: bool,
    pub draft_permission: Option<crate::settings::PermissionMode>,
    pub session_modes: HashMap<String, Option<crate::settings::PermissionMode>>,
    pub mode_sending: HashSet<String>,
    pub usage: HashMap<String, crate::api::features::UsageReport>,
    pub(crate) usage_status: FetchStatuses<String>,
    /// The user's folders, in sidebar order. Each is an opencode location.
    pub folders: Vec<PathBuf>,
    pub collapsed: HashSet<PathBuf>,
    /// Where "New session" creates sessions.
    pub current_folder: Option<PathBuf>,
    /// Root sessions in every location, most recently updated first.
    pub sessions: Vec<SessionInfo>,
    pub selected: Option<String>,
    pub busy: HashSet<String>,
    pub retry: HashMap<String, String>,
    /// Last error per session, cleared when a new prompt is sent.
    pub errors: HashMap<String, String>,
    pub conversations: HashMap<String, Conversation>,
    /// Recently selected sessions, newest last; their transcripts stay in
    /// memory (see [`Store::evict_conversations`]).
    pub(crate) recent: std::collections::VecDeque<String>,
    /// Why a session's history couldn't be fetched (cleared on success).
    pub load_errors: HashMap<String, String>,
    /// Pending prompts/compactions per session, oldest first. Kept whether
    /// or not the conversation is loaded; a user item moves into the
    /// transcript on `session.inbox.delivered`.
    pub inbox: HashMap<String, Vec<InboxItem>>,
    /// Subagent sessions by parent id, oldest first.
    pub children: HashMap<String, Vec<SessionInfo>>,
    /// Shell commands the server runs, by shell id.
    pub shells: HashMap<String, ShellInfo>,
    /// Per folder (or worktree directory): current branch.
    pub vcs: HashMap<PathBuf, VcsInfo>,
    /// Per sidebar folder: its project id and extra worktree checkouts.
    pub projects: HashMap<PathBuf, String>,
    pub worktrees: HashMap<PathBuf, Vec<PathBuf>>,
    /// Per folder: `/` menu sources.
    pub tooling: HashMap<PathBuf, Tooling>,
    /// Sessions whose transcript gained entries the stream doesn't describe
    /// (compactions, skills): re-fetched when their turn ends.
    pub needs_reload: HashSet<String>,
    pub connection: Option<Connection>,
    pub opencode_version: Option<String>,
    /// A transient, app-level problem shown as a dismissible banner.
    pub notice: Option<String>,
    /// Pending permission requests per session, their revisions (changed by
    /// live events and snapshots alike) and the user's replies in flight.
    pub(crate) permissions: Permissions,
    /// Pending question forms and their draft answers.
    pub(crate) forms: Forms,
    pub diffs: HashMap<PathBuf, Vec<FileDiff>>,
    pub(crate) diff_status: FetchStatuses<PathBuf>,
    /// Folders whose diff may be outdated (a turn ended while not shown).
    pub diff_stale: HashSet<PathBuf>,
    /// Per folder (location directory).
    pub catalogs: HashMap<PathBuf, Catalog>,
    /// The model/agent for the next new session. Updated by every pick, so
    /// the last choice sticks, like opencode's own TUI.
    pub draft_model: Option<ModelRef>,
    pub draft_agent: Option<String>,
}

impl EventEmitter<StoreEvent> for Store {}

impl Store {
    pub fn connection(&self) -> Connection {
        self.connection.clone().unwrap_or(Connection::Starting)
    }

    pub fn set_connection(&mut self, c: Connection, cx: &mut Context<Self>) {
        if self.connection.as_ref() != Some(&c) {
            self.connection = Some(c);
            cx.emit(StoreEvent::ConnectionChanged);
            cx.notify();
        }
    }

    /// Replace a session's pending permission requests (snapshot read).
    pub fn set_permissions(
        &mut self,
        sid: &str,
        requests: Vec<PermissionRequest>,
        cx: &mut Context<Self>,
    ) {
        self.permissions.replace(sid, requests);
        cx.emit(StoreEvent::StatusChanged {
            session_id: sid.to_string(),
        });
        cx.notify();
    }

    /// Replace the pending forms of a session (snapshot read). Drafts survive
    /// as long as the form stays pending.
    pub fn set_forms(&mut self, sid: &str, forms: Vec<FormRequest>, cx: &mut Context<Self>) {
        self.forms.replace(sid, forms);
        cx.emit(StoreEvent::StatusChanged {
            session_id: sid.to_string(),
        });
        cx.notify();
    }

    /// Stop showing replies whose outcome will never arrive (the stream or
    /// the server they were sent on is gone). Failed replies stay visible.
    pub(crate) fn abandon_replies(&mut self, cx: &mut Context<Self>) {
        let permissions = self.permissions.abandon_replies();
        let forms = self.forms.abandon_replies();
        if permissions || forms {
            cx.notify();
        }
    }

    /// One answer inside a form's draft; keyed by form so drafts survive
    /// re-renders.
    pub fn set_form_draft(
        &mut self,
        form_id: &str,
        key: &str,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        if self.forms.set_answer(form_id, key, value) {
            cx.notify();
        }
    }
}

/// Wall-clock epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests;
