//! App state: the single source of truth for folders, sessions and
//! conversations.
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
    /// True once the REST history has been fetched. Events for conversations
    /// that are not loaded are dropped; they are fetched on selection.
    pub loaded: bool,
    /// Highest durable sequence number folded in.
    pub last_seq: Option<u64>,
    /// A history fetch is in flight (see [`Store::begin_load`]).
    pub fetching: bool,
    /// Bumped per fetch; only the latest fetch's snapshot is applied.
    pub load_seq: u64,
    /// Messages that events touched while the fetch was in flight: the only
    /// local messages missing from the snapshot that survive it (others were
    /// reverted, compacted away or deleted on the server).
    pub live: HashSet<String>,
    /// Events for a not-yet-loaded conversation, replayed after its snapshot.
    pub pending: Vec<Envelope>,
}

/// Events buffered per loading conversation, at most.
const MAX_PENDING: usize = 20_000;

impl Conversation {
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
        primary()
            .find(|a| a.id == "build")
            .or_else(|| primary().next())
    }
}

#[derive(Default)]
pub struct Store {
    pub saved_grants: Vec<crate::api::features::SavedGrant>,
    pub saved_grants_loading: bool,
    pub saved_grants_error: Option<String>,
    pub grant_sending: HashSet<String>,
    pub preferences: crate::settings::Settings,
    pub unread: std::collections::BTreeSet<String>,
    pub window_active: bool,
    pub draft_permission: Option<crate::settings::PermissionMode>,
    pub session_modes: HashMap<String, Option<crate::settings::PermissionMode>>,
    pub mode_sending: HashSet<String>,
    pub usage: HashMap<String, crate::api::features::UsageReport>,
    pub usage_loading: HashSet<String>,
    pub usage_errors: HashMap<String, String>,
    pub review_base: HashMap<String, String>,
    pub review: HashMap<String, crate::api::features::ReviewMode>,
    pub commits: HashMap<String, Vec<crate::api::features::CommitSummary>>,
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
    pub permissions: HashMap<String, Vec<PermissionRequest>>,
    /// Changed by both live events and snapshots, to reject stale in-flight reads.
    pub permission_revision: HashMap<String, u64>,
    pub permission_sending: HashSet<(String, String)>,
    pub permission_errors: HashMap<(String, String), String>,
    /// Pending question forms, session-owned like permissions.
    pub forms: HashMap<String, Vec<FormRequest>>,
    pub form_revision: HashMap<String, u64>,
    pub form_sending: HashSet<(String, String)>,
    pub form_errors: HashMap<(String, String), String>,
    /// In-progress answers per pending form (`answer[key] = value`), seeded
    /// from the fields' defaults and edited as the user picks or types.
    pub form_drafts: HashMap<String, Value>,
    pub diffs: HashMap<String, Vec<FileDiff>>,
    pub diff_loading: HashSet<String>,
    pub diff_errors: HashMap<String, String>,
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

    pub fn permission_revision(&self, sid: &str) -> u64 {
        self.permission_revision
            .get(sid)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_permissions(
        &mut self,
        sid: &str,
        requests: Vec<PermissionRequest>,
        cx: &mut Context<Self>,
    ) {
        self.permissions.insert(
            sid.to_string(),
            requests
                .into_iter()
                .filter(|r| r.session_id == sid)
                .collect(),
        );
        let requests = &self.permissions[sid];
        self.permission_sending.retain(|(session, request)| {
            session != sid || requests.iter().any(|r| &r.id == request)
        });
        self.permission_errors.retain(|(session, request), _| {
            session != sid || requests.iter().any(|r| &r.id == request)
        });
        *self.permission_revision.entry(sid.to_string()).or_default() += 1;
        cx.emit(StoreEvent::StatusChanged {
            session_id: sid.to_string(),
        });
        cx.notify();
    }

    fn remove_permission(&mut self, sid: &str, rid: &str) {
        if let Some(requests) = self.permissions.get_mut(sid) {
            requests.retain(|r| r.id != rid);
        }
        let key = (sid.to_string(), rid.to_string());
        self.permission_sending.remove(&key);
        self.permission_errors.remove(&key);
        *self.permission_revision.entry(sid.to_string()).or_default() += 1;
    }

    pub fn form_revision(&self, sid: &str) -> u64 {
        self.form_revision.get(sid).copied().unwrap_or_default()
    }

    /// Replace the pending forms of a session (snapshot read). Drafts survive
    /// as long as the form stays pending.
    pub fn set_forms(&mut self, sid: &str, forms: Vec<FormRequest>, cx: &mut Context<Self>) {
        let forms: Vec<FormRequest> = forms.into_iter().filter(|f| f.session_id == sid).collect();
        for f in &forms {
            self.seed_form_draft(f);
        }
        self.forms.insert(sid.to_string(), forms.clone());
        let pending: HashSet<&str> = forms.iter().map(|f| f.id.as_str()).collect();
        self.form_sending
            .retain(|(session, form)| session != sid || pending.contains(form.as_str()));
        self.form_errors
            .retain(|(session, form), _| session != sid || pending.contains(form.as_str()));
        self.form_drafts
            .retain(|form, _| pending.contains(form.as_str()));
        *self.form_revision.entry(sid.to_string()).or_default() += 1;
        cx.emit(StoreEvent::StatusChanged {
            session_id: sid.to_string(),
        });
        cx.notify();
    }

    fn remove_form(&mut self, sid: &str, fid: &str) {
        if let Some(forms) = self.forms.get_mut(sid) {
            forms.retain(|f| f.id != fid);
        }
        let key = (sid.to_string(), fid.to_string());
        self.form_sending.remove(&key);
        self.form_errors.remove(&key);
        self.form_drafts.remove(fid);
        *self.form_revision.entry(sid.to_string()).or_default() += 1;
    }

    /// Fill a new form's draft with its fields' default answers.
    fn seed_form_draft(&mut self, form: &FormRequest) {
        if self.form_drafts.contains_key(&form.id) {
            return;
        }
        let mut draft = serde_json::Map::new();
        for field in &form.fields {
            if let Some(d) = &field.default {
                draft.insert(field.key.clone(), d.clone());
            }
        }
        self.form_drafts
            .insert(form.id.clone(), Value::Object(draft));
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
        let entry = self
            .form_drafts
            .entry(form_id.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(serde_json::Map::new());
        }
        let map = entry.as_object_mut().expect("draft object");
        if map.get(key) == Some(&value) {
            return;
        }
        map.insert(key.to_string(), value);
        cx.notify();
    }

    /// The current draft answers of a form (empty object if none yet).
    pub fn form_draft(&self, form_id: &str) -> Value {
        self.form_drafts
            .get(form_id)
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }

    pub fn set_catalog(&mut self, folder: PathBuf, catalog: Catalog, cx: &mut Context<Self>) {
        if self.catalogs.get(&folder) != Some(&catalog) {
            self.catalogs.insert(folder, catalog);
            cx.emit(StoreEvent::ChoicesChanged);
            cx.notify();
        }
    }

    /// The catalog of the folder the composer targets.
    pub fn target_catalog(&self) -> Option<&Catalog> {
        self.catalogs.get(&self.catalog_folder()?)
    }

    /// The sidebar folder whose catalog and `/` menu apply to the target
    /// folder (a worktree uses its repository's).
    pub fn catalog_folder(&self) -> Option<PathBuf> {
        let dir = self.target_folder()?;
        Some(self.folder_of(&dir).unwrap_or(dir))
    }

    /// The model the next prompt runs with: the selected session's, else the
    /// draft pick, else the folder default.
    pub fn effective_model(&self) -> Option<ModelRef> {
        let session = self.selected.as_ref().and_then(|id| self.session(id));
        match session {
            Some(s) => s.model.clone(),
            None => self.draft_model.clone(),
        }
        .or_else(|| self.target_catalog()?.default_model.clone())
    }

    /// The agent the next prompt runs with (same precedence as the model).
    pub fn effective_agent(&self) -> Option<String> {
        let session = self.selected.as_ref().and_then(|id| self.session(id));
        match session {
            Some(s) => s.agent.clone(),
            None => self.draft_agent.clone(),
        }
        .or_else(|| Some(self.target_catalog()?.default_agent()?.id.clone()))
    }

    pub fn set_draft_choice(
        &mut self,
        model: Option<ModelRef>,
        agent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if model.is_some() {
            self.draft_model = model;
        }
        if agent.is_some() {
            self.draft_agent = agent;
        }
        cx.emit(StoreEvent::ChoicesChanged);
        cx.notify();
    }

    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn sessions_in<'a>(
        &'a self,
        folder: &'a Path,
    ) -> impl Iterator<Item = &'a SessionInfo> + 'a {
        self.sessions.iter().filter(move |s| s.directory == folder)
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.busy.contains(session_id)
    }

    /// The session `delta` rows above (-1) or below (+1) the selected one,
    /// in sidebar order (sessions of expanded folders), wrapping around.
    /// With nothing selected, +1 picks the first and -1 the last.
    pub fn adjacent_session(&self, delta: isize) -> Option<String> {
        let order: Vec<&str> = self
            .folders
            .iter()
            .filter(|f| !self.collapsed.contains(*f))
            .flat_map(|f| self.sessions_in(f).map(|s| s.id.as_str()))
            .collect();
        if order.is_empty() {
            return None;
        }
        let n = order.len() as isize;
        let current = self
            .selected
            .as_deref()
            .and_then(|id| order.iter().position(|o| *o == id));
        let next = match current {
            Some(i) => (i as isize + delta).rem_euclid(n),
            None if delta < 0 => n - 1,
            None => 0,
        };
        Some(order[next as usize].to_string())
    }

    // ── M7: inbox, background tasks, folder info ─────────────────────────

    /// The M7 events. A delivered user item is rewritten into a
    /// `UserPrompt` and continues to the transcript code.
    fn apply_m7(&mut self, env: &mut Envelope) -> M7 {
        let inbox_changed = |sid: &str| {
            M7::Handled(vec![StoreEvent::InboxChanged {
                session_id: sid.to_string(),
            }])
        };
        match &env.event {
            Event::InboxEnqueued { session_id, item } => {
                let items = self.inbox.entry(session_id.clone()).or_default();
                if !items.iter().any(|i| i.id == item.id) {
                    items.push(item.clone());
                }
                inbox_changed(session_id)
            }
            Event::InboxCancelled {
                session_id,
                inbox_id,
            } => {
                self.remove_inbox_item(session_id, inbox_id);
                inbox_changed(session_id)
            }
            Event::InboxDeliveryChanged {
                session_id,
                inbox_id,
                delivery,
            } => {
                if let Some(item) = self
                    .inbox
                    .get_mut(session_id)
                    .and_then(|v| v.iter_mut().find(|i| &i.id == inbox_id))
                {
                    item.delivery = *delivery;
                }
                inbox_changed(session_id)
            }
            Event::InboxDelivered {
                session_id,
                inbox_id,
            } => {
                let (sid, id) = (session_id.clone(), inbox_id.clone());
                let item = self.remove_inbox_item(&sid, &id);
                let out = vec![StoreEvent::InboxChanged {
                    session_id: sid.clone(),
                }];
                match item {
                    Some(item) if item.kind == "user" => {
                        env.event = Event::UserPrompt {
                            session_id: sid,
                            message_id: id,
                            text: item.text,
                            attachments: item.attachments,
                        };
                        M7::Continue(out)
                    }
                    // A compaction (or an enqueue we missed): the REST
                    // projection fills the transcript when the turn ends.
                    _ => {
                        self.needs_reload.insert(sid);
                        M7::Handled(out)
                    }
                }
            }
            Event::SkillActivated { session_id } => {
                self.needs_reload.insert(session_id.clone());
                M7::Handled(vec![])
            }
            Event::ShellCreated(info) => {
                self.shells.insert(info.id.clone(), info.clone());
                M7::Handled(vec![StoreEvent::TasksChanged])
            }
            Event::ShellExited { id, status, exit } => {
                if let Some(sh) = self.shells.get_mut(id) {
                    sh.status = status.clone();
                    sh.exit = *exit;
                }
                M7::Handled(vec![StoreEvent::TasksChanged])
            }
            Event::ShellDeleted { id } => {
                // Keep finished shells visible (with their status) until the
                // next snapshot; drop only ones still marked running.
                if self.shells.get(id).is_some_and(ShellInfo::running) {
                    self.shells.remove(id);
                }
                M7::Handled(vec![StoreEvent::TasksChanged])
            }
            Event::BranchChanged { branch } => {
                let Some(dir) = env.directory.clone() else {
                    return M7::Handled(vec![]);
                };
                self.vcs.entry(dir).or_default().branch = branch.clone();
                M7::Handled(vec![StoreEvent::FolderInfoChanged])
            }
            _ => M7::Continue(vec![]),
        }
    }

    fn remove_inbox_item(&mut self, session_id: &str, item_id: &str) -> Option<InboxItem> {
        let items = self.inbox.get_mut(session_id)?;
        let pos = items.iter().position(|i| i.id == item_id)?;
        let item = items.remove(pos);
        if items.is_empty() {
            self.inbox.remove(session_id);
        }
        Some(item)
    }

    /// Pending items the user can still act on (prompts, not compactions).
    pub fn pending_prompts(&self, session_id: &str) -> &[InboxItem] {
        self.inbox.get(session_id).map_or(&[], Vec::as_slice)
    }

    pub fn set_inbox(&mut self, session_id: &str, items: Vec<InboxItem>, cx: &mut Context<Self>) {
        if items.is_empty() {
            self.inbox.remove(session_id);
        } else {
            self.inbox.insert(session_id.to_string(), items);
        }
        cx.emit(StoreEvent::InboxChanged {
            session_id: session_id.to_string(),
        });
        cx.notify();
    }

    pub fn set_children(
        &mut self,
        parent: &str,
        mut kids: Vec<SessionInfo>,
        cx: &mut Context<Self>,
    ) {
        kids.sort_by_key(|k| k.created);
        self.children.insert(parent.to_string(), kids);
        cx.emit(StoreEvent::TasksChanged);
        cx.notify();
    }

    /// Replace the shells of one folder with a snapshot.
    pub fn set_shells(&mut self, shells: Vec<ShellInfo>, cx: &mut Context<Self>) {
        for sh in shells {
            self.shells.insert(sh.id.clone(), sh);
        }
        cx.emit(StoreEvent::TasksChanged);
        cx.notify();
    }

    /// Subagent sessions of a session, oldest first.
    pub fn children_of(&self, session_id: &str) -> &[SessionInfo] {
        self.children.get(session_id).map_or(&[], Vec::as_slice)
    }

    /// Shells started by a session, newest first.
    pub fn shells_of(&self, session_id: &str) -> Vec<&ShellInfo> {
        let mut v: Vec<&ShellInfo> = self
            .shells
            .values()
            .filter(|s| s.session_id.as_deref() == Some(session_id))
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.started));
        v
    }

    /// Running subagents + shells of a session (the header badge).
    pub fn running_tasks(&self, session_id: &str) -> usize {
        self.children_of(session_id)
            .iter()
            .filter(|k| self.is_busy(&k.id))
            .count()
            + self
                .shells_of(session_id)
                .iter()
                .filter(|s| s.running())
                .count()
    }

    pub fn set_vcs(&mut self, dir: PathBuf, info: VcsInfo, cx: &mut Context<Self>) {
        self.vcs.insert(dir, info);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    pub fn set_worktrees(
        &mut self,
        folder: PathBuf,
        project_id: String,
        worktrees: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.projects.insert(folder.clone(), project_id);
        // The main checkout is the folder itself.
        let extra: Vec<PathBuf> = worktrees.into_iter().filter(|w| *w != folder).collect();
        self.worktrees.insert(folder, extra);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    pub fn set_tooling(&mut self, folder: PathBuf, tooling: Tooling, cx: &mut Context<Self>) {
        self.tooling.insert(folder, tooling);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    /// The sidebar folder a directory belongs to: itself, or the folder
    /// whose worktree it is.
    pub fn folder_of(&self, dir: &Path) -> Option<PathBuf> {
        if self.folders.iter().any(|f| f == dir) {
            return Some(dir.to_path_buf());
        }
        self.worktrees
            .iter()
            .find(|(_, wts)| wts.iter().any(|w| w == dir))
            .map(|(f, _)| f.clone())
    }

    /// The selected session's history hasn't arrived yet and there's nothing
    /// cached to show meanwhile.
    pub fn selected_is_loading(&self) -> bool {
        let Some(id) = self.selected.as_ref() else {
            return false;
        };
        !self.load_errors.contains_key(id)
            && self
                .conversations
                .get(id)
                .is_none_or(|c| !c.loaded && c.messages.is_empty())
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected.as_ref()?)
    }

    /// The folder a new session goes into: the selected session's folder,
    /// else the current folder, else the first folder.
    pub fn target_folder(&self) -> Option<PathBuf> {
        self.selected
            .as_ref()
            .and_then(|id| self.session(id))
            .map(|s| s.directory.clone())
            .or_else(|| self.current_folder.clone())
            .or_else(|| self.folders.first().cloned())
    }

    pub fn select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected != id {
            if let Some(dir) = id
                .as_ref()
                .and_then(|i| self.session(i))
                .map(|s| s.directory.clone())
            {
                self.current_folder = Some(dir);
            }
            self.selected = id;
            cx.emit(StoreEvent::SelectionChanged);
            cx.notify();
        }
    }

    pub fn set_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.folders = folders;
        if self
            .current_folder
            .as_ref()
            .is_none_or(|c| !self.folders.contains(c))
        {
            self.current_folder = self.folders.first().cloned();
        }
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }

    pub fn set_current_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.current_folder = Some(folder);
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }

    pub fn toggle_collapsed(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_path_buf());
        }
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }

    pub fn set_sessions(&mut self, mut sessions: Vec<SessionInfo>, cx: &mut Context<Self>) {
        sessions.retain(|s| s.parent_id.is_none());
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.sessions = sessions;
        if let Some(sel) = &self.selected
            && self.session(sel).is_none()
        {
            self.selected = None;
            cx.emit(StoreEvent::SelectionChanged);
        }
        cx.emit(StoreEvent::SessionsChanged);
        cx.notify();
    }

    /// Replace busy state with the server's view (`GET /api/session/active`).
    pub fn set_active(&mut self, active: Vec<String>, cx: &mut Context<Self>) {
        let new: HashSet<String> = active.into_iter().collect();
        let changed: Vec<String> = self.busy.symmetric_difference(&new).cloned().collect();
        self.busy = new;
        let events = changed
            .into_iter()
            .map(|session_id| StoreEvent::StatusChanged { session_id })
            .collect();
        self.emit_all(events, cx);
    }

    pub fn set_busy(&mut self, session_id: &str, busy: bool, cx: &mut Context<Self>) {
        let events = self.set_busy_inner(session_id, busy);
        self.emit_all(events, cx);
    }

    pub fn clear_error(&mut self, session_id: &str) {
        self.errors.remove(session_id);
        self.retry.remove(session_id);
    }

    /// Fold one event. Emits [`StoreEvent`]s and notifies when anything changed.
    pub fn apply(&mut self, env: Envelope, cx: &mut Context<Self>) {
        let events = self.apply_inner(env);
        self.emit_all(events, cx);
    }

    /// Replace a conversation with the authoritative REST projection,
    /// merging so that text streamed after the snapshot was taken survives.
    pub fn reconcile(&mut self, session_id: &str, history: Vec<Message>, cx: &mut Context<Self>) {
        self.load_errors.remove(session_id);
        let events = self.reconcile_inner(session_id, history);
        self.emit_all(events, cx);
    }

    /// A history fetch for `session_id` starts; returns its ticket for
    /// [`Store::finish_load`]. Events that arrive meanwhile are tracked (and,
    /// for an unloaded conversation, buffered) so the snapshot can be merged
    /// without losing or resurrecting anything.
    pub fn begin_load(&mut self, session_id: &str) -> u64 {
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        conv.load_seq += 1;
        conv.fetching = true;
        conv.live.clear();
        conv.pending.clear();
        conv.load_seq
    }

    /// Apply the snapshot of fetch `ticket`, unless a newer fetch started.
    pub fn finish_load(
        &mut self,
        session_id: &str,
        ticket: u64,
        history: anyhow::Result<Vec<Message>>,
        cx: &mut Context<Self>,
    ) {
        let Some(conv) = self.conversations.get_mut(session_id) else {
            return;
        };
        if conv.load_seq != ticket {
            return;
        }
        match history {
            Ok(history) => self.reconcile(session_id, history, cx),
            Err(e) => {
                conv.fetching = false;
                conv.pending.clear();
                self.load_errors
                    .insert(session_id.to_string(), format!("{e:#}"));
                cx.notify();
            }
        }
    }

    /// Forget fetched history for every session except `keep`, so it is
    /// re-fetched when next selected (used after a reconnect).
    pub fn invalidate_except(&mut self, keep: Option<&str>) {
        for (id, conv) in self.conversations.iter_mut() {
            if Some(id.as_str()) != keep {
                conv.loaded = false;
            }
        }
    }

    fn emit_all(&mut self, events: Vec<StoreEvent>, cx: &mut Context<Self>) {
        if events.is_empty() {
            return;
        }
        for e in events {
            cx.emit(e);
        }
        cx.notify();
    }

    fn set_busy_inner(&mut self, session_id: &str, busy: bool) -> Vec<StoreEvent> {
        let changed = if busy {
            self.busy.insert(session_id.to_string())
        } else {
            self.busy.remove(session_id)
        };
        if !busy {
            self.retry.remove(session_id);
        }
        if changed {
            vec![StoreEvent::StatusChanged {
                session_id: session_id.to_string(),
            }]
        } else {
            vec![]
        }
    }

    pub(crate) fn apply_inner(&mut self, mut env: Envelope) -> Vec<StoreEvent> {
        // Inbox, background tasks and folder info are session-/folder-level.
        let mut out = match self.apply_m7(&mut env) {
            M7::Handled(events) => return events,
            M7::Continue(events) => events,
        };
        // Session-level state first (applies whether or not the conversation
        // is loaded).
        match &env.event {
            Event::SessionUpserted(s) => {
                if let Some(parent) = &s.parent_id {
                    let kids = self.children.entry(parent.clone()).or_default();
                    match kids.iter_mut().find(|k| k.id == s.id) {
                        Some(k) => {
                            let title = std::mem::take(&mut k.title);
                            *k = s.clone();
                            if k.title.is_empty() {
                                k.title = title;
                            }
                        }
                        None => kids.push(s.clone()),
                    }
                    out.push(StoreEvent::TasksChanged);
                } else {
                    let old = self.session(&s.id).cloned();
                    self.sessions.retain(|x| x.id != s.id);
                    let mut s = s.clone();
                    // `session.created` carries neither title nor choices.
                    if let Some(old) = old {
                        if s.title.is_empty() {
                            s.title = old.title;
                        }
                        s.model = s.model.or(old.model);
                        s.agent = s.agent.or(old.agent);
                    }
                    let pos = self
                        .sessions
                        .iter()
                        .position(|x| x.updated <= s.updated)
                        .unwrap_or(self.sessions.len());
                    self.sessions.insert(pos, s);
                    out.push(StoreEvent::SessionsChanged);
                }
                return out;
            }
            Event::SessionRenamed { session_id, title } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| &s.id == session_id) {
                    s.title = title.clone();
                    out.push(StoreEvent::SessionsChanged);
                }
                for kid in self.children.values_mut().flatten() {
                    if &kid.id == session_id {
                        kid.title = title.clone();
                        out.push(StoreEvent::TasksChanged);
                    }
                }
                return out;
            }
            Event::SessionRemoved { session_id } => {
                self.sessions.retain(|x| &x.id != session_id);
                self.conversations.remove(session_id);
                self.busy.remove(session_id);
                self.permissions.remove(session_id);
                self.permission_revision.remove(session_id);
                self.permission_sending.retain(|(sid, _)| sid != session_id);
                self.permission_errors
                    .retain(|(sid, _), _| sid != session_id);
                self.forms.remove(session_id);
                self.form_revision.remove(session_id);
                self.form_sending.retain(|(sid, _)| sid != session_id);
                self.form_errors.retain(|(sid, _), _| sid != session_id);
                self.diffs.remove(session_id);
                self.diff_loading.remove(session_id);
                self.diff_errors.remove(session_id);
                self.inbox.remove(session_id);
                self.children.remove(session_id);
                out.push(StoreEvent::SessionsChanged);
                if self.selected.as_ref() == Some(session_id) {
                    self.selected = None;
                    out.push(StoreEvent::SelectionChanged);
                }
                return out;
            }
            Event::ModelSelected { session_id, model } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| &s.id == session_id) {
                    s.model = Some(model.clone());
                    out.push(StoreEvent::ChoicesChanged);
                }
                return out;
            }
            Event::AgentSelected { session_id, agent } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| &s.id == session_id) {
                    s.agent = Some(agent.clone());
                    out.push(StoreEvent::ChoicesChanged);
                }
                return out;
            }
            Event::PermissionAsked(request) => {
                let sid = request.session_id.clone();
                let requests = self.permissions.entry(sid.clone()).or_default();
                requests.retain(|r| r.id != request.id);
                requests.push(request.clone());
                *self.permission_revision.entry(sid.clone()).or_default() += 1;
                return vec![StoreEvent::StatusChanged { session_id: sid }];
            }
            Event::PermissionReplied {
                session_id,
                request_id,
            } => {
                self.remove_permission(session_id, request_id);
                return vec![StoreEvent::StatusChanged {
                    session_id: session_id.clone(),
                }];
            }
            Event::FormAsked(form) => {
                let sid = form.session_id.clone();
                let fid = form.id.clone();
                self.seed_form_draft(form);
                if let Some(forms) = self.forms.get_mut(&sid) {
                    forms.retain(|f| f.id != fid);
                    forms.push(form.clone());
                } else {
                    self.forms.insert(sid.clone(), vec![form.clone()]);
                }
                *self.form_revision.entry(sid.clone()).or_default() += 1;
                return vec![StoreEvent::StatusChanged { session_id: sid }];
            }
            Event::FormReplied {
                session_id,
                form_id,
            }
            | Event::FormCancelled {
                session_id,
                form_id,
            } => {
                self.remove_form(session_id, form_id);
                return vec![StoreEvent::StatusChanged {
                    session_id: session_id.clone(),
                }];
            }
            Event::ExecutionStarted { session_id } => {
                self.errors.remove(session_id);
                return self.set_busy_inner(session_id, true);
            }
            Event::ExecutionEnded {
                session_id,
                outcome,
            } => {
                if let Outcome::Failed { error } = outcome {
                    self.errors.insert(session_id.clone(), error.clone());
                }
                return self.set_busy_inner(session_id, false);
            }
            Event::RetryScheduled {
                session_id,
                message,
            } => {
                self.retry.insert(session_id.clone(), message.clone());
                out.push(StoreEvent::StatusChanged {
                    session_id: session_id.clone(),
                });
                return out;
            }
            _ => {}
        }

        let Some(session_id) = event_session(&env.event).map(str::to_string) else {
            return out;
        };
        let Some(conv) = self.conversations.get_mut(&session_id) else {
            return out;
        };
        if !conv.loaded {
            // Replayed onto the snapshot when it lands.
            if conv.fetching && conv.pending.len() < MAX_PENDING {
                conv.pending.push(env);
            }
            return out;
        }
        if conv.fetching
            && let Some(id) = event_message(&env.event)
        {
            conv.live.insert(id.to_string());
        }
        if let Some(seq) = env.seq {
            conv.last_seq = Some(conv.last_seq.map_or(seq, |s| s.max(seq)));
        }
        // Server time of the event; local events (demo, replies) use now.
        let at = if env.created > 0 {
            env.created
        } else {
            now_ms()
        };
        let changed = |index: usize, inserted: bool| {
            if inserted {
                StoreEvent::MessageInserted {
                    session_id: session_id.clone(),
                    index,
                }
            } else {
                StoreEvent::MessageChanged {
                    session_id: session_id.clone(),
                    index,
                }
            }
        };

        match env.event {
            Event::UserPrompt {
                message_id,
                text,
                attachments,
                ..
            } => {
                if let Err(index) = conv.index_of(&message_id) {
                    conv.messages.insert(
                        index,
                        Message::User(UserMessage {
                            id: message_id,
                            text,
                            attachments,
                            meta: Meta {
                                created: Some(at),
                                ..Default::default()
                            },
                        }),
                    );
                    out.push(changed(index, true));
                }
            }
            Event::StepStarted {
                message_id,
                model,
                agent,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                msg.meta.created.get_or_insert(at);
                if model.is_some() {
                    msg.meta.model = model;
                }
                if agent.is_some() {
                    msg.meta.agent = agent;
                }
                if inserted {
                    out.push(changed(index, true));
                }
            }
            Event::BlockStarted {
                message_id,
                kind,
                ordinal,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                prose_mut(msg, kind, ordinal).2.created.get_or_insert(at);
                out.push(changed(index, inserted));
            }
            Event::BlockDelta {
                message_id,
                kind,
                ordinal,
                delta,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let (text, _, _) = prose_mut(msg, kind, ordinal);
                text.push_str(&delta);
                out.push(changed(index, inserted));
            }
            Event::BlockEnded {
                message_id,
                kind,
                ordinal,
                text: full,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let (text, done, time) = prose_mut(msg, kind, ordinal);
                *text = full;
                *done = true;
                time.completed = Some(at);
                out.push(changed(index, inserted));
            }
            Event::ToolStarted {
                message_id,
                call_id,
                name,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                tool_mut(msg, &call_id).name = name;
                out.push(changed(index, inserted));
            }
            Event::ToolCalled {
                message_id,
                call_id,
                input,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.input = input;
                t.status = ToolStatus::Running;
                out.push(changed(index, inserted));
            }
            Event::ToolSucceeded {
                message_id,
                call_id,
                output,
                metadata,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.status = ToolStatus::Completed;
                t.output = output;
                t.metadata = metadata;
                out.push(changed(index, inserted));
            }
            Event::ToolProgress {
                message_id,
                call_id,
                metadata,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                if let (Some(current), Some(update)) =
                    (t.metadata.as_object_mut(), metadata.as_object())
                {
                    current.extend(update.clone());
                } else {
                    t.metadata = metadata;
                }
                out.push(changed(index, inserted));
            }
            Event::ToolFailed {
                message_id,
                call_id,
                error,
                output,
                metadata,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.status = ToolStatus::Error;
                t.error = Some(error);
                t.output = output;
                t.metadata = metadata;
                out.push(changed(index, inserted));
            }
            Event::StepEnded {
                message_id,
                finish,
                tokens,
                cost,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                msg.completed = true;
                msg.finish = Some(finish);
                msg.meta.completed = Some(at);
                if let Some(t) = tokens {
                    msg.meta.tokens = Some(crate::api::features::Tokens::parse(&t));
                }
                if cost.is_some() {
                    msg.meta.cost = cost;
                }
                out.push(changed(index, inserted));
            }
            Event::StepFailed {
                message_id, error, ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                msg.completed = true;
                msg.meta.completed = Some(at);
                msg.finish = Some("error".into());
                msg.error = Some(error);
                out.push(changed(index, inserted));
            }
            _ => {}
        }
        out
    }

    pub(crate) fn reconcile_inner(
        &mut self,
        session_id: &str,
        history: Vec<Message>,
    ) -> Vec<StoreEvent> {
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        // The snapshot's order is authoritative (delivery order).
        let old = std::mem::take(&mut conv.messages);
        let in_snapshot: HashSet<String> = history.iter().map(|m| m.id().to_string()).collect();
        let mut local: HashMap<String, Message> = HashMap::new();
        let mut newer: Vec<Message> = Vec::new();
        for m in old {
            if in_snapshot.contains(m.id()) {
                local.insert(m.id().to_string(), m);
            } else {
                newer.push(m);
            }
        }
        let mut merged: Vec<Message> = history
            .into_iter()
            .map(|mut snap| {
                let prev = local.remove(snap.id());
                if let (Message::Assistant(s), Some(Message::Assistant(l))) = (&mut snap, prev) {
                    merge_blocks(&mut s.blocks, l.blocks);
                }
                snap
            })
            .collect();
        // Messages streamed while the fetch was in flight survive, after the
        // snapshot, in the order they arrived; other local-only messages are
        // gone on the server (reverted, compacted, deleted).
        merged.extend(newer.into_iter().filter(|m| conv.live.contains(m.id())));
        conv.messages = merged;
        conv.loaded = true;
        conv.fetching = false;
        conv.live.clear();
        let pending = std::mem::take(&mut conv.pending);
        // Prose blocks the snapshot has: buffered deltas for them partly
        // overlap its text, so they are joined by overlap, not appended.
        let in_snapshot: HashSet<(String, BlockKind, u64)> = conv
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            })
            .flat_map(|a| {
                a.blocks.iter().filter_map(|b| match b {
                    Block::Prose { kind, ordinal, .. } => Some((a.id.clone(), *kind, *ordinal)),
                    _ => None,
                })
            })
            .collect();
        let mut joined: Vec<((String, BlockKind, u64), String)> = Vec::new();
        for env in pending {
            if let Event::BlockDelta {
                message_id,
                kind,
                ordinal,
                delta,
                ..
            } = &env.event
            {
                let key = (message_id.clone(), *kind, *ordinal);
                if in_snapshot.contains(&key) {
                    match joined.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, d)) => d.push_str(delta),
                        None => joined.push((key, delta.clone())),
                    }
                    continue;
                }
            }
            self.apply_inner(env);
        }
        if let Some(conv) = self.conversations.get_mut(session_id) {
            for ((message_id, kind, ordinal), deltas) in joined {
                if let Ok(i) = conv.index_of(&message_id)
                    && let Message::Assistant(a) = &mut conv.messages[i]
                {
                    let (text, _, _) = prose_mut(a, kind, ordinal);
                    append_overlapping(text, &deltas);
                }
            }
        }
        vec![StoreEvent::MessagesReset {
            session_id: session_id.to_string(),
        }]
    }
}

/// Append `deltas` to `text`, skipping the prefix of `deltas` that `text`
/// already ends with (the part the snapshot captured).
fn append_overlapping(text: &mut String, deltas: &str) {
    let max = deltas.len().min(text.len());
    let k = (0..=max)
        .rev()
        .filter(|&k| deltas.is_char_boundary(k))
        .find(|&k| text.ends_with(&deltas[..k]))
        .unwrap_or(0);
    text.push_str(&deltas[k..]);
}

/// The message a transcript event is about.
fn event_message(e: &Event) -> Option<&str> {
    Some(match e {
        Event::UserPrompt { message_id, .. }
        | Event::StepStarted { message_id, .. }
        | Event::BlockStarted { message_id, .. }
        | Event::BlockDelta { message_id, .. }
        | Event::BlockEnded { message_id, .. }
        | Event::ToolStarted { message_id, .. }
        | Event::ToolCalled { message_id, .. }
        | Event::ToolProgress { message_id, .. }
        | Event::ToolSucceeded { message_id, .. }
        | Event::ToolFailed { message_id, .. }
        | Event::StepEnded { message_id, .. }
        | Event::StepFailed { message_id, .. } => message_id,
        _ => return None,
    })
}

fn event_session(e: &Event) -> Option<&str> {
    Some(match e {
        Event::UserPrompt { session_id, .. }
        | Event::StepStarted { session_id, .. }
        | Event::BlockStarted { session_id, .. }
        | Event::BlockDelta { session_id, .. }
        | Event::BlockEnded { session_id, .. }
        | Event::ToolStarted { session_id, .. }
        | Event::ToolCalled { session_id, .. }
        | Event::ToolProgress { session_id, .. }
        | Event::ToolSucceeded { session_id, .. }
        | Event::ToolFailed { session_id, .. }
        | Event::StepEnded { session_id, .. }
        | Event::StepFailed { session_id, .. } => session_id,
        _ => return None,
    })
}

fn prose_mut(
    msg: &mut AssistantMessage,
    kind: BlockKind,
    ordinal: u64,
) -> (&mut String, &mut bool, &mut Timing) {
    let pos = msg.blocks.iter().position(
        |b| matches!(b, Block::Prose { kind: k, ordinal: o, .. } if *k == kind && *o == ordinal),
    );
    let pos = pos.unwrap_or_else(|| {
        msg.blocks.push(Block::Prose {
            kind,
            ordinal,
            text: String::new(),
            done: false,
            time: Timing::default(),
        });
        msg.blocks.len() - 1
    });
    match &mut msg.blocks[pos] {
        Block::Prose {
            text, done, time, ..
        } => (text, done, time),
        Block::Tool(_) => unreachable!(),
    }
}

fn tool_mut<'a>(msg: &'a mut AssistantMessage, call_id: &str) -> &'a mut ToolCall {
    let pos = msg
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Tool(t) if t.call_id == call_id));
    let pos = pos.unwrap_or_else(|| {
        msg.blocks.push(Block::Tool(Box::new(ToolCall {
            call_id: call_id.to_string(),
            name: String::new(),
            status: ToolStatus::Pending,
            input: serde_json::Value::Null,
            output: String::new(),
            error: None,
            metadata: serde_json::Value::Null,
        })));
        msg.blocks.len() - 1
    });
    match &mut msg.blocks[pos] {
        Block::Tool(t) => t,
        Block::Prose { .. } => unreachable!(),
    }
}

/// Snapshot blocks win, except where local streamed text strictly extends
/// the snapshot's (the server persists text slightly after streaming it),
/// and local blocks the snapshot doesn't know yet are kept.
fn merge_blocks(snapshot: &mut Vec<Block>, local: Vec<Block>) {
    for lb in local {
        let found = snapshot.iter_mut().find(|sb| same_block(sb, &lb));
        match (found, &lb) {
            (Some(Block::Prose { text: s, .. }), Block::Prose { text: l, .. }) => {
                if l.len() > s.len() && l.starts_with(s.as_str()) {
                    *s = l.clone();
                }
            }
            (Some(_), _) => {}
            (None, _) => snapshot.push(lb),
        }
    }
}

fn same_block(a: &Block, b: &Block) -> bool {
    match (a, b) {
        (
            Block::Prose {
                kind: ka,
                ordinal: oa,
                ..
            },
            Block::Prose {
                kind: kb,
                ordinal: ob,
                ..
            },
        ) => ka == kb && oa == ob,
        (Block::Tool(x), Block::Tool(y)) => x.call_id == y.call_id,
        _ => false,
    }
}

/// Wall-clock epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::Delivery;

    fn fixture() -> Vec<Envelope> {
        include_str!("../tests/fixtures/v2-events.jsonl")
            .lines()
            .filter_map(Envelope::from_json)
            .collect()
    }

    fn sid() -> String {
        fixture()
            .into_iter()
            .find_map(|e| match e.event {
                Event::SessionUpserted(s) => Some(s.id),
                _ => None,
            })
            .unwrap()
    }

    fn session_in(id: &str, dir: &str, updated: u64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: String::new(),
            directory: PathBuf::from(dir),
            created: updated,
            updated,
            parent_id: None,
            model: None,
            agent: None,
        }
    }

    fn wire(json: &str) -> Envelope {
        Envelope::from_json(json).unwrap()
    }

    #[test]
    fn queued_prompt_reaches_transcript_only_on_delivery() {
        let mut s = Store::default();
        s.reconcile_inner("ses_a", vec![]);
        let enq = |id: &str, text: &str, d: &str| {
            wire(&format!(
                r#"{{"type":"session.inbox.enqueued","data":{{"inboxID":"{id}","sessionID":"ses_a","item":{{"type":"user","payload":{{"text":"{text}"}},"delivery":"{d}"}}}}}}"#
            ))
        };
        s.apply_inner(enq("msg_q", "later", "queue"));
        s.apply_inner(enq("msg_x", "drop me", "queue"));
        assert_eq!(s.pending_prompts("ses_a").len(), 2);
        assert!(
            s.conversations["ses_a"].messages.is_empty(),
            "pending isn't transcript"
        );
        // Promote, cancel, deliver.
        s.apply_inner(wire(r#"{"type":"session.inbox.delivery.changed","data":{"sessionID":"ses_a","inboxID":"msg_q","delivery":"steer"}}"#));
        assert_eq!(s.pending_prompts("ses_a")[0].delivery, Delivery::Steer);
        s.apply_inner(wire(
            r#"{"type":"session.inbox.cancelled","data":{"sessionID":"ses_a","inboxID":"msg_x"}}"#,
        ));
        let events = s.apply_inner(wire(
            r#"{"type":"session.inbox.delivered","data":{"sessionID":"ses_a","inboxID":"msg_q"}}"#,
        ));
        assert!(s.pending_prompts("ses_a").is_empty());
        assert!(events.contains(&StoreEvent::InboxChanged {
            session_id: "ses_a".into()
        }));
        assert!(matches!(&s.conversations["ses_a"].messages[..],
            [Message::User(u)] if u.id == "msg_q" && u.text == "later"));
        // A delivery we never saw enqueued is left to the next reconcile.
        s.apply_inner(wire(r#"{"type":"session.inbox.delivered","data":{"sessionID":"ses_a","inboxID":"msg_unknown"}}"#));
        assert_eq!(s.conversations["ses_a"].messages.len(), 1);
    }

    #[test]
    fn transcript_keeps_delivery_order_not_id_order() {
        // Observed on 2.2.6: a queued prompt (smaller id) sent before a steer
        // is delivered after it; the REST projection uses delivery order.
        let mut s = Store::default();
        s.reconcile_inner("ses_a", vec![]);
        for (id, d) in [("msg_1_queue", "queue"), ("msg_2_steer", "steer")] {
            s.apply_inner(wire(&format!(
                r#"{{"type":"session.inbox.enqueued","data":{{"inboxID":"{id}","sessionID":"ses_a","item":{{"type":"user","payload":{{"text":"{id}"}},"delivery":"{d}"}}}}}}"#
            )));
        }
        for id in ["msg_2_steer", "msg_1_queue"] {
            s.apply_inner(wire(&format!(
                r#"{{"type":"session.inbox.delivered","data":{{"sessionID":"ses_a","inboxID":"{id}"}}}}"#
            )));
        }
        let ids = |s: &Store| -> Vec<String> {
            s.conversations["ses_a"]
                .messages
                .iter()
                .map(|m| m.id().to_string())
                .collect()
        };
        assert_eq!(ids(&s), ["msg_2_steer", "msg_1_queue"]);
        // A snapshot in the same order changes nothing; a message streamed
        // after the snapshot stays after it.
        let user = |id: &str| {
            Message::User(UserMessage {
                id: id.into(),
                text: id.into(),
                attachments: vec![],
                meta: Default::default(),
            })
        };
        s.begin_load("ses_a");
        s.apply_inner(wire(r#"{"type":"session.step.started","data":{"sessionID":"ses_a","assistantMessageID":"msg_0_reply"}}"#));
        s.reconcile_inner("ses_a", vec![user("msg_2_steer"), user("msg_1_queue")]);
        assert_eq!(ids(&s), ["msg_2_steer", "msg_1_queue", "msg_0_reply"]);
        // Local messages no event touched during the fetch are gone on the
        // server (an undo): the next snapshot drops them.
        s.begin_load("ses_a");
        s.reconcile_inner("ses_a", vec![user("msg_2_steer")]);
        assert_eq!(ids(&s), ["msg_2_steer"]);
    }

    #[test]
    fn deltas_buffered_during_a_load_join_the_snapshot_without_gaps_or_repeats() {
        let mut s = Store::default();
        let sid = "ses_b";
        s.begin_load(sid);
        let delta = |d: &str| {
            wire(&format!(
                r#"{{"type":"session.text.delta","data":{{"sessionID":"{sid}","assistantMessageID":"msg_a","ordinal":0,"delta":"{d}"}}}}"#
            ))
        };
        // Arrive while the fetch is in flight; the snapshot saw "Hel".
        for d in ["He", "l", "lo", " world"] {
            s.apply_inner(delta(d));
        }
        let snap = Message::Assistant(AssistantMessage {
            id: "msg_a".into(),
            blocks: vec![Block::Prose {
                kind: BlockKind::Text,
                ordinal: 0,
                text: "Hel".into(),
                done: true,
                time: Default::default(),
            }],
            ..Default::default()
        });
        s.reconcile_inner(sid, vec![snap]);
        let Message::Assistant(a) = &s.conversations[sid].messages[0] else {
            panic!()
        };
        let Block::Prose { text, .. } = &a.blocks[0] else {
            panic!()
        };
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn a_stale_load_is_ignored() {
        let mut s = Store::default();
        let first = s.begin_load("ses_c");
        let second = s.begin_load("ses_c");
        assert_ne!(first, second);
        assert_eq!(s.conversations["ses_c"].load_seq, second);
    }

    #[test]
    fn subagents_and_shells_are_tracked_per_parent() {
        let mut s = Store::default();
        s.apply_inner(wire(r#"{"type":"session.created","created":5,"data":{"sessionID":"ses_c","parentID":"ses_p","title":"Look around","agent":"general","location":{"directory":"/p"}}}"#));
        assert!(s.sessions.is_empty(), "children stay out of the sidebar");
        assert_eq!(s.children_of("ses_p")[0].title, "Look around");
        s.apply_inner(wire(
            r#"{"type":"session.execution.started","data":{"sessionID":"ses_c"}}"#,
        ));
        s.apply_inner(wire(r#"{"type":"shell.created","data":{"info":{"id":"sh_1","status":"running","command":"sleep 20","metadata":{"sessionID":"ses_p"},"time":{"started":1}}}}"#));
        assert_eq!(s.running_tasks("ses_p"), 2);
        s.apply_inner(wire(
            r#"{"type":"shell.exited","data":{"id":"sh_1","status":"killed"}}"#,
        ));
        s.apply_inner(wire(
            r#"{"type":"session.execution.succeeded","data":{"sessionID":"ses_c"}}"#,
        ));
        assert_eq!(s.running_tasks("ses_p"), 0);
        assert_eq!(s.shells_of("ses_p")[0].status, "killed");
        // Branch updates are keyed by the event's location.
        s.apply_inner(wire(r#"{"type":"vcs.branch.updated","location":{"directory":"/p"},"data":{"branch":"feat"}}"#));
        assert_eq!(s.vcs[Path::new("/p")].branch.as_deref(), Some("feat"));
    }

    #[test]
    fn adjacent_session_follows_sidebar_order() {
        let mut s = Store {
            folders: vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ],
            // Sidebar order: folders in order, sessions newest first.
            sessions: vec![
                session_in("a2", "/a", 2),
                session_in("a1", "/a", 1),
                session_in("b1", "/b", 1),
                session_in("c1", "/c", 1),
            ],
            ..Default::default()
        };
        assert_eq!(s.adjacent_session(1).as_deref(), Some("a2"));
        assert_eq!(s.adjacent_session(-1).as_deref(), Some("c1"));
        s.selected = Some("a1".into());
        assert_eq!(s.adjacent_session(1).as_deref(), Some("b1"));
        assert_eq!(s.adjacent_session(-1).as_deref(), Some("a2"));
        // Collapsed folders are skipped; the ends wrap around.
        s.collapsed.insert(PathBuf::from("/b"));
        assert_eq!(s.adjacent_session(1).as_deref(), Some("c1"));
        s.selected = Some("c1".into());
        assert_eq!(s.adjacent_session(1).as_deref(), Some("a2"));
        s.collapsed
            .extend([PathBuf::from("/a"), PathBuf::from("/c")]);
        assert_eq!(s.adjacent_session(1), None);
    }

    #[test]
    fn loading_and_load_errors() {
        let mut s = Store::default();
        assert!(!s.selected_is_loading());
        s.selected = Some("x".into());
        assert!(s.selected_is_loading());
        s.load_errors.insert("x".into(), "boom".into());
        assert!(!s.selected_is_loading());
        s.load_errors.clear();
        s.reconcile_inner("x", vec![]);
        assert!(!s.selected_is_loading());
    }

    fn loaded_store() -> Store {
        let mut s = Store::default();
        s.reconcile_inner(&sid(), vec![]);
        s
    }

    fn summary(m: &Message) -> Vec<String> {
        match m {
            Message::User(u) => vec![u.text.clone()],
            Message::Assistant(a) => a
                .blocks
                .iter()
                .map(|b| match b {
                    Block::Prose { text, .. } => text.clone(),
                    Block::Tool(t) => format!("{}:{:?}:{}", t.name, t.status, t.output),
                })
                .chain(a.error.clone())
                .collect(),
            Message::Marker(m) => vec![m.text.clone()],
        }
    }

    fn permission(sid: &str, rid: &str) -> PermissionRequest {
        PermissionRequest::from_json(&serde_json::json!({
            "id":rid, "sessionID":sid, "action":"shell", "resources":["echo hello"],
            "save":["echo *"], "metadata":{"command":"echo hello"}
        }))
        .unwrap()
    }

    #[test]
    fn forms_are_session_scoped_and_close_cleanly() {
        let mut s = Store::default();
        let form = || {
            FormRequest::from_json(&serde_json::json!({
                "id": "frm_1", "sessionID": "ses_a", "title": "Questions",
                "fields": [
                    {"key": "q0", "type": "string", "options": [{"value": "y", "label": "Yes"}], "custom": true},
                    {"key": "q1", "type": "string", "options": [{"value": "startup", "label": "Startup"}]}
                ]
            }))
            .unwrap()
        };
        s.apply_inner(Envelope::bare(Event::FormAsked(form())));
        s.apply_inner(Envelope::bare(Event::FormAsked(form())));
        assert_eq!(
            s.forms["ses_a"].len(),
            1,
            "duplicate ask frames must not duplicate cards"
        );
        assert_eq!(s.form_draft("frm_1"), serde_json::json!({}));
        let before = s.form_revision("ses_a");
        s.apply_inner(Envelope::bare(Event::FormReplied {
            session_id: "ses_a".into(),
            form_id: "frm_1".into(),
        }));
        assert!(s.forms["ses_a"].is_empty());
        assert!(
            s.form_revision("ses_a") > before,
            "in-flight snapshots must be invalidated"
        );
        // The draft and scratch of the closed form go away with it.
        assert_eq!(s.form_draft("frm_1"), serde_json::json!({}));
    }

    #[test]
    fn permissions_are_session_scoped_and_survive_unloaded_history() {
        let mut s = Store::default();
        for (sid, rid) in [
            ("ses_a", "perm_1"),
            ("ses_b", "perm_2"),
            ("ses_a", "perm_1"),
        ] {
            s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(sid, rid))));
        }
        assert_eq!(
            s.permissions["ses_a"].len(),
            1,
            "duplicate frames must not duplicate cards"
        );
        assert_eq!(s.permissions["ses_b"].len(), 1);
        let before = s.permission_revision("ses_a");
        s.apply_inner(Envelope::bare(Event::PermissionReplied {
            session_id: "ses_a".into(),
            request_id: "perm_1".into(),
        }));
        assert!(s.permissions["ses_a"].is_empty());
        assert!(
            s.permission_revision("ses_a") > before,
            "in-flight snapshots must be invalidated"
        );
        assert_eq!(s.permissions["ses_b"].len(), 1);
        assert!(s.conversations.is_empty());
    }

    #[test]
    fn deleting_session_cleans_pending_replies_and_diffs() {
        let mut s = Store::default();
        s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(
            "ses_a", "perm_1",
        ))));
        s.permission_sending
            .insert(("ses_a".into(), "perm_1".into()));
        s.diffs.insert("ses_a".into(), vec![]);
        s.apply_inner(Envelope::bare(Event::SessionRemoved {
            session_id: "ses_a".into(),
        }));
        assert!(!s.permissions.contains_key("ses_a"));
        assert!(s.permission_sending.is_empty());
        assert!(s.diffs.is_empty());
    }

    #[test]
    fn tool_failure_preserves_output_and_metadata_from_wire() {
        let mut s = loaded_store();
        let sid = sid();
        let wire = serde_json::json!({"type":"session.tool.failed", "data":{
            "sessionID":sid, "assistantMessageID":"msg_z", "id":"call_z",
            "error":{"type":"shell","message":"Command failed"},
            "content":[{"type":"text","text":"stderr output"}],
            "metadata":{"exit":2,"truncated":true}
        }});
        s.apply_inner(Envelope::from_json(&wire.to_string()).unwrap());
        let Message::Assistant(a) = s.conversations[&sid].messages.last().unwrap() else {
            panic!()
        };
        let Block::Tool(t) = &a.blocks[0] else {
            panic!()
        };
        assert_eq!(t.status, ToolStatus::Error);
        assert_eq!(t.output, "stderr output");
        assert_eq!(t.metadata["exit"], 2);
        assert_eq!(t.error.as_deref(), Some("Command failed"));
        let projected = Message::from_v2_json(&serde_json::json!({
            "id":"msg_z","type":"assistant","content":[{"type":"tool","id":"call_z","name":"",
            "state":{"status":"error","input":null,"error":{"message":"Command failed"},
            "content":[{"type":"text","text":"stderr output"}],"metadata":{"exit":2,"truncated":true}}}]
        })).unwrap();
        let Message::Assistant(remote) = projected else {
            panic!()
        };
        assert_eq!(a.blocks, remote.blocks);
    }

    #[test]
    fn replaying_captured_events_matches_rest_projection() {
        let mut s = loaded_store();
        for env in fixture() {
            s.apply_inner(env);
        }
        // The REST projection of the same session after both turns (the
        // second was interrupted by the user).
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/v2-messages.json")).unwrap();
        let rest: Vec<Message> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Message::from_v2_json)
            .collect();
        let live = &s.conversations[&sid()].messages;
        assert_eq!(live.len(), rest.len());
        for (l, r) in live.iter().zip(&rest) {
            assert_eq!(l.id(), r.id());
            assert_eq!(summary(l), summary(r));
        }
        assert!(!s.is_busy(&sid()));
        assert!(
            !s.errors.contains_key(&sid()),
            "an interrupt is not an error"
        );
        assert!(
            !s.session(&sid()).unwrap().title.is_empty(),
            "session.renamed sets the title"
        );
    }

    #[test]
    fn busy_follows_execution_lifecycle() {
        let mut s = loaded_store();
        let mut busy_during_tool = false;
        for env in fixture() {
            let tool_step =
                matches!(&env.event, Event::StepEnded { finish, .. } if finish == "tool-calls");
            s.apply_inner(env);
            if tool_step {
                busy_during_tool = s.is_busy(&sid());
            }
        }
        assert!(
            busy_during_tool,
            "busy until session.execution.* ends the turn"
        );
        assert!(!s.is_busy(&sid()));
    }

    #[test]
    fn events_for_unloaded_sessions_only_touch_session_state() {
        let mut s = Store::default();
        for env in fixture().into_iter().take(12) {
            s.apply_inner(env);
        }
        assert!(!s.conversations.contains_key(&sid()));
        assert!(s.is_busy(&sid()));
        assert_eq!(s.sessions.len(), 1);
    }

    #[test]
    fn failed_execution_records_error() {
        let mut s = loaded_store();
        let id = sid();
        s.apply_inner(Envelope::bare(Event::ExecutionStarted {
            session_id: id.clone(),
        }));
        s.apply_inner(Envelope::bare(Event::ExecutionEnded {
            session_id: id.clone(),
            outcome: Outcome::Failed {
                error: "Model not found".into(),
            },
        }));
        assert!(!s.is_busy(&id));
        assert_eq!(s.errors[&id], "Model not found");
    }

    #[test]
    fn reconcile_keeps_local_text_that_extends_snapshot() {
        let mut s = loaded_store();
        let id = sid();
        let block = |t: &str| Block::Prose {
            kind: BlockKind::Text,
            ordinal: 0,
            text: t.into(),
            done: false,
            time: Default::default(),
        };
        let msg = |t: &str| {
            Message::Assistant(AssistantMessage {
                id: "msg_2".into(),
                blocks: vec![block(t)],
                ..Default::default()
            })
        };
        let text = |s: &Store| match &s.conversations[&id].messages[0] {
            Message::Assistant(a) => summary(&Message::Assistant(a.clone()))[0].clone(),
            _ => unreachable!(),
        };
        s.reconcile_inner(&id, vec![msg("Hello wor")]);
        s.reconcile_inner(&id, vec![msg("Hello")]);
        assert_eq!(text(&s), "Hello wor");
        s.reconcile_inner(&id, vec![msg("Hello world")]);
        assert_eq!(text(&s), "Hello world");
    }

    #[test]
    fn target_folder_prefers_selected_session() {
        let mut s = Store {
            folders: vec!["/a".into(), "/b".into()],
            current_folder: Some("/a".into()),
            ..Default::default()
        };
        s.sessions.push(SessionInfo {
            id: "ses_x".into(),
            title: String::new(),
            directory: "/b".into(),
            created: 0,
            updated: 0,
            parent_id: None,
            model: None,
            agent: None,
        });
        assert_eq!(s.target_folder(), Some(PathBuf::from("/a")));
        s.selected = Some("ses_x".into());
        assert_eq!(s.target_folder(), Some(PathBuf::from("/b")));
    }

    #[test]
    fn model_and_agent_choices_follow_events_and_precedence() {
        use crate::api::model::{AgentInfo, AgentMode};
        let mref = |id: &str| ModelRef {
            provider_id: "p".into(),
            id: id.into(),
            variant: None,
        };
        let mut s = Store {
            folders: vec!["/a".into()],
            ..Default::default()
        };
        s.catalogs.insert(
            "/a".into(),
            Catalog {
                models: vec![],
                default_model: Some(mref("default")),
                agents: vec![
                    AgentInfo {
                        id: "plan".into(),
                        name: "Plan".into(),
                        mode: AgentMode::Primary,
                        hidden: false,
                    },
                    AgentInfo {
                        id: "build".into(),
                        name: "Build".into(),
                        mode: AgentMode::Primary,
                        hidden: false,
                    },
                ],
            },
        );
        // Nothing chosen: folder defaults, `build` preferred.
        assert_eq!(s.effective_model(), Some(mref("default")));
        assert_eq!(s.effective_agent().as_deref(), Some("build"));
        // A draft pick applies to new sessions.
        s.draft_model = Some(mref("draft"));
        assert_eq!(s.effective_model(), Some(mref("draft")));

        let session = SessionInfo {
            id: "ses_x".into(),
            title: "t".into(),
            directory: "/a".into(),
            created: 0,
            updated: 0,
            parent_id: None,
            model: None,
            agent: None,
        };
        s.apply_inner(Envelope::bare(Event::SessionUpserted(session.clone())));
        s.selected = Some("ses_x".into());
        // A session without its own model shows the folder default, not the draft.
        assert_eq!(s.effective_model(), Some(mref("default")));
        let ev = s.apply_inner(Envelope::bare(Event::ModelSelected {
            session_id: "ses_x".into(),
            model: mref("chosen"),
        }));
        assert_eq!(ev, vec![StoreEvent::ChoicesChanged]);
        s.apply_inner(Envelope::bare(Event::AgentSelected {
            session_id: "ses_x".into(),
            agent: "plan".into(),
        }));
        assert_eq!(s.effective_model(), Some(mref("chosen")));
        assert_eq!(s.effective_agent().as_deref(), Some("plan"));
        // `session.created` replays carry no choices; keep the known ones.
        s.apply_inner(Envelope::bare(Event::SessionUpserted(session)));
        assert_eq!(s.effective_model(), Some(mref("chosen")));
        assert_eq!(s.session("ses_x").unwrap().title, "t");
    }
}
