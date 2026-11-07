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

use crate::api::model::{
    AssistantMessage, Block, BlockKind, Message, SessionInfo, ToolCall, ToolStatus, UserMessage,
};
use crate::api::{Envelope, Event};

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
    /// Created through the v1 API: no v2 history, shown read-only.
    pub legacy: bool,
    /// Highest durable sequence number folded in.
    pub last_seq: Option<u64>,
}

impl Conversation {
    fn index_of(&self, id: &str) -> Result<usize, usize> {
        self.messages.binary_search_by(|m| m.id().cmp(id))
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
            Message::User(_) => unreachable!("message id {id} is a user message"),
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
}

#[derive(Default)]
pub struct Store {
    /// The user's folders, in sidebar order. Each is an opencode location.
    pub folders: Vec<PathBuf>,
    pub collapsed: HashSet<PathBuf>,
    /// Where "New session" creates sessions.
    pub current_folder: Option<PathBuf>,
    /// Root sessions in every location, most recently updated first.
    pub sessions: Vec<SessionInfo>,
    pub selected: Option<String>,
    pub busy: HashSet<String>,
    pub retry: HashMap<String, (u64, String)>,
    /// Last error per session, cleared when a new prompt is sent.
    pub errors: HashMap<String, String>,
    pub conversations: HashMap<String, Conversation>,
    pub connection: Option<Connection>,
    pub opencode_version: Option<String>,
    /// A transient, app-level problem shown as a dismissible banner.
    pub notice: Option<String>,
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
    pub fn reconcile(
        &mut self,
        session_id: &str,
        history: Vec<Message>,
        legacy: bool,
        cx: &mut Context<Self>,
    ) {
        let events = self.reconcile_inner(session_id, history, legacy);
        self.emit_all(events, cx);
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

    pub(crate) fn apply_inner(&mut self, env: Envelope) -> Vec<StoreEvent> {
        let mut out = Vec::new();
        // Session-level state first (applies whether or not the conversation
        // is loaded).
        match &env.event {
            Event::SessionUpserted(s) => {
                if s.parent_id.is_none() {
                    self.sessions.retain(|x| x.id != s.id);
                    let pos = self
                        .sessions
                        .iter()
                        .position(|x| x.updated <= s.updated)
                        .unwrap_or(self.sessions.len());
                    self.sessions.insert(pos, s.clone());
                    out.push(StoreEvent::SessionsChanged);
                }
                return out;
            }
            Event::SessionDeleted { session_id } => {
                self.sessions.retain(|x| &x.id != session_id);
                self.conversations.remove(session_id);
                self.busy.remove(session_id);
                out.push(StoreEvent::SessionsChanged);
                if self.selected.as_ref() == Some(session_id) {
                    self.selected = None;
                    out.push(StoreEvent::SelectionChanged);
                }
                return out;
            }
            Event::Prompted { session_id, .. } | Event::StepStarted { session_id, .. } => {
                out.extend(self.set_busy_inner(session_id, true));
            }
            Event::StepEnded {
                session_id, finish, ..
            } if finish != "tool-calls" => {
                out.extend(self.set_busy_inner(session_id, false));
            }
            Event::StepFailed {
                session_id, error, ..
            } => {
                out.extend(self.set_busy_inner(session_id, false));
                if error != "Provider turn interrupted" {
                    self.errors.insert(session_id.clone(), error.clone());
                }
            }
            Event::Retried {
                session_id,
                attempt,
                message,
            } => {
                self.retry
                    .insert(session_id.clone(), (*attempt, message.clone()));
                out.push(StoreEvent::StatusChanged {
                    session_id: session_id.clone(),
                });
            }
            _ => {}
        }

        let Some(session_id) = event_session(&env.event).map(str::to_string) else {
            return out;
        };
        let Some(conv) = self.conversations.get_mut(&session_id).filter(|c| c.loaded) else {
            return out;
        };
        if let Some(seq) = env.seq {
            conv.last_seq = Some(conv.last_seq.map_or(seq, |s| s.max(seq)));
        }
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
            Event::Prompted {
                message_id, text, ..
            } => {
                if let Err(index) = conv.index_of(&message_id) {
                    conv.messages.insert(
                        index,
                        Message::User(UserMessage {
                            id: message_id,
                            text,
                        }),
                    );
                    out.push(changed(index, true));
                }
            }
            Event::StepStarted { message_id, .. } => {
                let (index, inserted, _) = conv.assistant_mut(&message_id);
                if inserted {
                    out.push(changed(index, true));
                }
            }
            Event::BlockStarted {
                message_id,
                kind,
                block_id,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                prose_mut(msg, kind, &block_id);
                out.push(changed(index, inserted));
            }
            Event::BlockDelta {
                message_id,
                kind,
                block_id,
                delta,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let (text, _) = prose_mut(msg, kind, &block_id);
                text.push_str(&delta);
                out.push(changed(index, inserted));
            }
            Event::BlockEnded {
                message_id,
                kind,
                block_id,
                text: full,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let (text, done) = prose_mut(msg, kind, &block_id);
                *text = full;
                *done = true;
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
                name,
                input,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.name = name;
                t.input = input;
                t.status = ToolStatus::Running;
                out.push(changed(index, inserted));
            }
            Event::ToolSucceeded {
                message_id,
                call_id,
                output,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.status = ToolStatus::Completed;
                t.output = output;
                out.push(changed(index, inserted));
            }
            Event::ToolFailed {
                message_id,
                call_id,
                error,
                ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                let t = tool_mut(msg, &call_id);
                t.status = ToolStatus::Error;
                t.error = Some(error);
                out.push(changed(index, inserted));
            }
            Event::StepEnded {
                message_id, finish, ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                msg.completed = true;
                msg.finish = Some(finish);
                out.push(changed(index, inserted));
            }
            Event::StepFailed {
                message_id, error, ..
            } => {
                let (index, inserted, msg) = conv.assistant_mut(&message_id);
                msg.completed = true;
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
        legacy: bool,
    ) -> Vec<StoreEvent> {
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        let mut old: HashMap<String, Message> = std::mem::take(&mut conv.messages)
            .into_iter()
            .map(|m| (m.id().to_string(), m))
            .collect();
        let mut merged: Vec<Message> = history
            .into_iter()
            .map(|mut snap| {
                let local = old.remove(snap.id());
                if let (Message::Assistant(s), Some(Message::Assistant(l))) = (&mut snap, local) {
                    merge_blocks(&mut s.blocks, l.blocks);
                }
                snap
            })
            .collect();
        // Messages created after the snapshot was taken survive.
        merged.extend(old.into_values());
        merged.sort_by(|a, b| a.id().cmp(b.id()));
        conv.messages = merged;
        conv.loaded = true;
        conv.legacy = legacy;
        vec![StoreEvent::MessagesReset {
            session_id: session_id.to_string(),
        }]
    }
}

fn event_session(e: &Event) -> Option<&str> {
    Some(match e {
        Event::Prompted { session_id, .. }
        | Event::StepStarted { session_id, .. }
        | Event::BlockStarted { session_id, .. }
        | Event::BlockDelta { session_id, .. }
        | Event::BlockEnded { session_id, .. }
        | Event::ToolStarted { session_id, .. }
        | Event::ToolCalled { session_id, .. }
        | Event::ToolSucceeded { session_id, .. }
        | Event::ToolFailed { session_id, .. }
        | Event::StepEnded { session_id, .. }
        | Event::StepFailed { session_id, .. } => session_id,
        _ => return None,
    })
}

fn prose_mut<'a>(
    msg: &'a mut AssistantMessage,
    kind: BlockKind,
    block_id: &str,
) -> (&'a mut String, &'a mut bool) {
    let pos = msg.blocks.iter().position(
        |b| matches!(b, Block::Prose { kind: k, id, .. } if *k == kind && id == block_id),
    );
    let pos = pos.unwrap_or_else(|| {
        msg.blocks.push(Block::Prose {
            kind,
            id: block_id.to_string(),
            text: String::new(),
            done: false,
        });
        msg.blocks.len() - 1
    });
    match &mut msg.blocks[pos] {
        Block::Prose { text, done, .. } => (text, done),
        Block::Tool(_) => unreachable!(),
    }
}

fn tool_mut<'a>(msg: &'a mut AssistantMessage, call_id: &str) -> &'a mut ToolCall {
    let pos = msg
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Tool(t) if t.call_id == call_id));
    let pos = pos.unwrap_or_else(|| {
        msg.blocks.push(Block::Tool(ToolCall {
            call_id: call_id.to_string(),
            name: String::new(),
            status: ToolStatus::Pending,
            input: serde_json::Value::Null,
            output: String::new(),
            error: None,
        }));
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
                kind: ka, id: ia, ..
            },
            Block::Prose {
                kind: kb, id: ib, ..
            },
        ) => ka == kb && ia == ib,
        (Block::Tool(x), Block::Tool(y)) => x.call_id == y.call_id,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "ses_e4a80e891ffeMVuHnRk3ssjEfA";

    fn fixture() -> Vec<Envelope> {
        include_str!("../tests/fixtures/v2-events.jsonl")
            .lines()
            .filter_map(Envelope::from_json)
            .collect()
    }

    fn loaded_store() -> Store {
        let mut s = Store::default();
        s.reconcile_inner(SID, vec![], false);
        s
    }

    fn assistant(s: &Store, i: usize) -> &AssistantMessage {
        match &s.conversations[SID].messages[i] {
            Message::Assistant(a) => a,
            m => panic!("expected assistant, got {m:?}"),
        }
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
                .collect(),
        }
    }

    #[test]
    fn replaying_captured_events_matches_rest_projection() {
        let mut s = loaded_store();
        for env in fixture() {
            s.apply_inner(env);
        }
        // The REST projection of the same session, captured after the first
        // two turns (the fixture then has an interrupted third turn).
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/v2-messages.json")).unwrap();
        let mut rest: Vec<Message> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Message::from_v2_json)
            .collect();
        rest.sort_by(|a, b| a.id().cmp(b.id()));
        let live = &s.conversations[SID].messages;
        assert!(live.len() > rest.len());
        for (l, r) in live.iter().zip(&rest) {
            assert_eq!(l.id(), r.id());
            assert_eq!(summary(l), summary(r));
        }
        assert!(!s.is_busy(SID));
        assert!(!s.errors.contains_key(SID), "an interrupt is not an error");
    }

    #[test]
    fn busy_follows_steps() {
        let mut s = loaded_store();
        let mut saw_busy_after_tool_step = false;
        for env in fixture() {
            let tool_step =
                matches!(&env.event, Event::StepEnded { finish, .. } if finish == "tool-calls");
            s.apply_inner(env);
            if tool_step {
                saw_busy_after_tool_step = s.is_busy(SID);
            }
        }
        assert!(
            saw_busy_after_tool_step,
            "a tool-calls step keeps the session busy"
        );
    }

    #[test]
    fn events_for_unloaded_sessions_only_touch_status() {
        let mut s = Store::default();
        for env in fixture().into_iter().take(12) {
            s.apply_inner(env);
        }
        assert!(!s.conversations.contains_key(SID));
        assert!(s.is_busy(SID));
        assert_eq!(s.sessions.len(), 1);
    }

    #[test]
    fn reconcile_keeps_local_text_that_extends_snapshot() {
        let mut s = loaded_store();
        let block = |t: &str| Block::Prose {
            kind: BlockKind::Text,
            id: "text-0".into(),
            text: t.into(),
            done: false,
        };
        let msg = |t: &str| {
            Message::Assistant(AssistantMessage {
                id: "msg_2".into(),
                blocks: vec![block(t)],
                ..Default::default()
            })
        };
        s.reconcile_inner(SID, vec![msg("Hello wor")], false);
        s.reconcile_inner(SID, vec![msg("Hello")], false);
        assert!(
            matches!(&assistant(&s, 0).blocks[0], Block::Prose { text, .. } if text == "Hello wor")
        );
        s.reconcile_inner(SID, vec![msg("Hello world")], false);
        assert!(
            matches!(&assistant(&s, 0).blocks[0], Block::Prose { text, .. } if text == "Hello world")
        );
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
        });
        assert_eq!(s.target_folder(), Some(PathBuf::from("/a")));
        s.selected = Some("ses_x".into());
        assert_eq!(s.target_folder(), Some(PathBuf::from("/b")));
    }
}
