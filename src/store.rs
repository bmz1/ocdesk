//! App state: the single source of truth for sessions, messages and parts.
//!
//! The API layer folds SSE events into it with [`Store::apply`] and replaces a
//! session's history with [`Store::reconcile`] after every (re)connect. Views
//! observe the `Entity<Store>` and subscribe to [`StoreEvent`] to remeasure
//! only what changed. Views never mutate it directly; they go through the
//! controller.

use std::collections::HashMap;

use gpui_kit::{Context, EventEmitter};
use opencode_codes::protocol_generated::types as oc;

use crate::api::Event;
use crate::api::events::RunState;

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

pub struct MessageEntry {
    pub info: oc::Message,
    /// Sorted by part id (ids are time-ordered).
    pub parts: Vec<oc::Part>,
}

impl MessageEntry {
    pub fn id(&self) -> &str {
        message_id(&self.info)
    }

    pub fn is_user(&self) -> bool {
        matches!(self.info, oc::Message::User(_))
    }
}

#[derive(Default)]
pub struct Conversation {
    /// Sorted by message id (ids are time-ordered).
    pub messages: Vec<MessageEntry>,
    /// True once the REST history has been fetched; events for conversations
    /// that are not loaded are dropped (they are fetched on selection).
    pub loaded: bool,
}

impl Conversation {
    fn index_of(&self, id: &str) -> Result<usize, usize> {
        self.messages.binary_search_by(|m| m.id().cmp(id))
    }
}

/// What changed, so views can do the minimum amount of relayout.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreEvent {
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
    MessageRemoved {
        session_id: String,
        index: usize,
    },
    /// Content of one row changed (a part was added, grew, or was updated).
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
    /// Root sessions, most recently updated first.
    pub sessions: Vec<oc::Session>,
    pub selected: Option<String>,
    pub status: HashMap<String, RunState>,
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

    pub fn session(&self, id: &str) -> Option<&oc::Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        matches!(
            self.status.get(session_id),
            Some(RunState::Busy | RunState::Retry { .. })
        )
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected.as_ref()?)
    }

    pub fn select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected != id {
            self.selected = id;
            cx.emit(StoreEvent::SelectionChanged);
            cx.notify();
        }
    }

    pub fn set_sessions(&mut self, mut sessions: Vec<oc::Session>, cx: &mut Context<Self>) {
        sessions.retain(|s| s.parent_id.is_none());
        sessions.sort_by_key(|s| std::cmp::Reverse(s.time.updated));
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

    pub fn set_status(&mut self, session_id: &str, state: RunState, cx: &mut Context<Self>) {
        let changes = self.set_status_inner(session_id, state);
        self.emit_all(changes, cx);
    }

    pub fn clear_error(&mut self, session_id: &str) {
        self.errors.remove(session_id);
    }

    /// Fold one event. Emits [`StoreEvent`]s and notifies when anything changed.
    pub fn apply(&mut self, ev: Event, cx: &mut Context<Self>) {
        let changes = self.apply_inner(ev);
        self.emit_all(changes, cx);
    }

    /// Replace a conversation with the authoritative REST history, merging so
    /// that text streamed after the snapshot was taken is not lost.
    pub fn reconcile(
        &mut self,
        session_id: &str,
        history: Vec<oc::MessageWithParts>,
        cx: &mut Context<Self>,
    ) {
        let changes = self.reconcile_inner(session_id, history);
        self.emit_all(changes, cx);
    }

    fn emit_all(&mut self, changes: Vec<StoreEvent>, cx: &mut Context<Self>) {
        if changes.is_empty() {
            return;
        }
        for c in changes {
            cx.emit(c);
        }
        cx.notify();
    }

    fn set_status_inner(&mut self, session_id: &str, state: RunState) -> Vec<StoreEvent> {
        if self.status.get(session_id) == Some(&state) {
            return vec![];
        }
        if state == RunState::Idle {
            self.status.remove(session_id);
        } else {
            self.status.insert(session_id.to_string(), state);
        }
        vec![StoreEvent::StatusChanged {
            session_id: session_id.to_string(),
        }]
    }

    pub(crate) fn apply_inner(&mut self, ev: Event) -> Vec<StoreEvent> {
        match ev {
            Event::SessionUpserted(s) => {
                if s.parent_id.is_some() {
                    return vec![];
                }
                self.sessions.retain(|x| x.id != s.id);
                let pos = self
                    .sessions
                    .iter()
                    .position(|x| x.time.updated <= s.time.updated)
                    .unwrap_or(self.sessions.len());
                self.sessions.insert(pos, s);
                vec![StoreEvent::SessionsChanged]
            }
            Event::SessionDeleted { session_id } => {
                self.sessions.retain(|x| x.id != session_id);
                self.conversations.remove(&session_id);
                self.status.remove(&session_id);
                let mut out = vec![StoreEvent::SessionsChanged];
                if self.selected.as_deref() == Some(&session_id) {
                    self.selected = None;
                    out.push(StoreEvent::SelectionChanged);
                }
                out
            }
            Event::SessionStatus { session_id, state } => self.set_status_inner(&session_id, state),
            Event::SessionIdle { session_id } => self.set_status_inner(&session_id, RunState::Idle),
            Event::SessionError {
                session_id: Some(sid),
                message,
            } => {
                // The server can send the same error twice (second with a
                // stack trace); keep the first line of the first one.
                let first = message.lines().next().unwrap_or_default().to_string();
                if !self.errors.contains_key(&sid) {
                    self.errors.insert(sid.clone(), first);
                }
                vec![StoreEvent::StatusChanged { session_id: sid }]
            }
            Event::MessageUpserted { session_id, info } => {
                let Some(conv) = self.loaded_mut(&session_id) else {
                    return vec![];
                };
                let id = message_id(&info).to_string();
                match conv.index_of(&id) {
                    Ok(index) => {
                        conv.messages[index].info = info;
                        vec![StoreEvent::MessageChanged { session_id, index }]
                    }
                    Err(index) => {
                        conv.messages.insert(
                            index,
                            MessageEntry {
                                info,
                                parts: vec![],
                            },
                        );
                        vec![StoreEvent::MessageInserted { session_id, index }]
                    }
                }
            }
            Event::MessageRemoved {
                session_id,
                message_id,
            } => {
                let Some(conv) = self.loaded_mut(&session_id) else {
                    return vec![];
                };
                match conv.index_of(&message_id) {
                    Ok(index) => {
                        conv.messages.remove(index);
                        vec![StoreEvent::MessageRemoved { session_id, index }]
                    }
                    Err(_) => vec![],
                }
            }
            Event::PartUpserted { session_id, part } => {
                let Some(conv) = self.loaded_mut(&session_id) else {
                    return vec![];
                };
                let mid = part_message_id(&part).to_string();
                let (index, mut out) = match conv.index_of(&mid) {
                    Ok(i) => (i, vec![]),
                    Err(i) => {
                        // Part arrived before its message.updated: create a
                        // placeholder from the part so nothing is dropped.
                        let Some(info) = placeholder_message(&session_id, &mid, &part) else {
                            return vec![];
                        };
                        conv.messages.insert(
                            i,
                            MessageEntry {
                                info,
                                parts: vec![],
                            },
                        );
                        (
                            i,
                            vec![StoreEvent::MessageInserted {
                                session_id: session_id.clone(),
                                index: i,
                            }],
                        )
                    }
                };
                let parts = &mut conv.messages[index].parts;
                let pid = part_id(&part).to_string();
                match parts.binary_search_by(|p| part_id(p).cmp(&pid)) {
                    Ok(i) => parts[i] = part,
                    Err(i) => parts.insert(i, part),
                }
                out.push(StoreEvent::MessageChanged { session_id, index });
                out
            }
            Event::PartDelta {
                session_id,
                message_id,
                part_id: pid,
                field,
                delta,
            } => {
                let Some(conv) = self.loaded_mut(&session_id) else {
                    return vec![];
                };
                let Ok(index) = conv.index_of(&message_id) else {
                    return vec![];
                };
                let parts = &mut conv.messages[index].parts;
                let Ok(i) = parts.binary_search_by(|p| part_id(p).cmp(&pid)) else {
                    return vec![];
                };
                if field != "text" {
                    return vec![];
                }
                match &mut parts[i] {
                    oc::Part::Text(t) => t.text.push_str(&delta),
                    oc::Part::Reasoning(r) => r.text.push_str(&delta),
                    _ => return vec![],
                }
                vec![StoreEvent::MessageChanged { session_id, index }]
            }
            Event::PartRemoved {
                session_id,
                message_id,
                part_id: pid,
            } => {
                let Some(conv) = self.loaded_mut(&session_id) else {
                    return vec![];
                };
                let Ok(index) = conv.index_of(&message_id) else {
                    return vec![];
                };
                conv.messages[index].parts.retain(|p| part_id(p) != pid);
                vec![StoreEvent::MessageChanged { session_id, index }]
            }
            Event::SessionError {
                session_id: None, ..
            }
            | Event::PermissionAsked(_)
            | Event::PermissionReplied { .. }
            | Event::Connected
            | Event::Disconnected
            | Event::Other(_) => vec![],
        }
    }

    fn loaded_mut(&mut self, session_id: &str) -> Option<&mut Conversation> {
        self.conversations.get_mut(session_id).filter(|c| c.loaded)
    }

    pub(crate) fn reconcile_inner(
        &mut self,
        session_id: &str,
        history: Vec<oc::MessageWithParts>,
    ) -> Vec<StoreEvent> {
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        let mut old: HashMap<String, MessageEntry> = std::mem::take(&mut conv.messages)
            .into_iter()
            .map(|m| (m.id().to_string(), m))
            .collect();

        let mut merged: Vec<MessageEntry> = history
            .into_iter()
            .map(|mwp| {
                let mut parts = mwp.parts;
                if let Some(local) = old.remove(message_id(&mwp.info)) {
                    merge_parts(&mut parts, local.parts);
                }
                parts.sort_by(|a, b| part_id(a).cmp(part_id(b)));
                MessageEntry {
                    info: mwp.info,
                    parts,
                }
            })
            .collect();
        // Messages created after the snapshot was taken survive.
        merged.extend(old.into_values());
        merged.sort_by(|a, b| a.id().cmp(b.id()));

        conv.messages = merged;
        conv.loaded = true;
        vec![StoreEvent::MessagesReset {
            session_id: session_id.to_string(),
        }]
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
}

/// Snapshot parts win, except where local streamed text strictly extends the
/// snapshot's text (deltas that arrived after the snapshot was taken), and
/// local parts the snapshot doesn't know about yet are kept.
fn merge_parts(snapshot: &mut Vec<oc::Part>, local: Vec<oc::Part>) {
    for lp in local {
        match snapshot.iter_mut().find(|sp| part_id(sp) == part_id(&lp)) {
            Some(sp) => {
                if let (Some(s_text), Some(l_text)) = (part_text(sp), part_text(&lp))
                    && l_text.len() > s_text.len()
                    && l_text.starts_with(s_text)
                {
                    *sp = lp;
                }
            }
            None => snapshot.push(lp),
        }
    }
}

pub fn part_text(p: &oc::Part) -> Option<&str> {
    match p {
        oc::Part::Text(t) => Some(&t.text),
        oc::Part::Reasoning(r) => Some(&r.text),
        _ => None,
    }
}

pub fn message_id(m: &oc::Message) -> &str {
    match m {
        oc::Message::User(u) => &u.id,
        oc::Message::Assistant(a) => &a.id,
    }
}

pub fn part_id(p: &oc::Part) -> &str {
    use oc::Part::*;
    match p {
        Text(x) => &x.id,
        Subtask(x) => &x.id,
        Reasoning(x) => &x.id,
        File(x) => &x.id,
        Tool(x) => &x.id,
        StepStart(x) => &x.id,
        StepFinish(x) => &x.id,
        Snapshot(x) => &x.id,
        Patch(x) => &x.id,
        Agent(x) => &x.id,
        Retry(x) => &x.id,
        Compaction(x) => &x.id,
    }
}

pub fn part_message_id(p: &oc::Part) -> &str {
    use oc::Part::*;
    match p {
        Text(x) => &x.message_id,
        Subtask(x) => &x.message_id,
        Reasoning(x) => &x.message_id,
        File(x) => &x.message_id,
        Tool(x) => &x.message_id,
        StepStart(x) => &x.message_id,
        StepFinish(x) => &x.message_id,
        Snapshot(x) => &x.message_id,
        Patch(x) => &x.message_id,
        Agent(x) => &x.message_id,
        Retry(x) => &x.message_id,
        Compaction(x) => &x.message_id,
    }
}

/// Assistant placeholder for a part whose `message.updated` hasn't arrived.
/// The real info replaces it as soon as it does.
fn placeholder_message(
    session_id: &str,
    message_id: &str,
    _part: &oc::Part,
) -> Option<oc::Message> {
    serde_json::from_value(serde_json::json!({
        "role": "assistant", "id": message_id, "sessionID": session_id,
        "agent": "", "cost": 0, "mode": "", "modelID": "", "providerID": "",
        "parentID": "", "path": {"cwd": "", "root": ""},
        "time": {"created": 0},
        "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }))
    .ok()
}

/// First line of the `data.message` of an opencode error object.
pub fn error_message(err: &impl serde::Serialize) -> String {
    let v = serde_json::to_value(err).unwrap_or_default();
    let msg = v
        .pointer("/data/message")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("name").and_then(|n| n.as_str()))
        .unwrap_or("error");
    msg.lines().next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "ses_1";

    fn ev(json: serde_json::Value) -> Event {
        let oc_ev: oc::Event = serde_json::from_value(json).expect("valid event");
        Event::from_stream(opencode_codes::StreamEvent::Event(Box::new(oc_ev)))
    }

    fn assistant(id: &str) -> serde_json::Value {
        serde_json::json!({"type":"message.updated","id":"evt","properties":{"sessionID":SID,"info":{
            "id":id,"parentID":"msg_0","role":"assistant","mode":"build","agent":"build",
            "path":{"cwd":"/","root":"/"},"cost":0,
            "tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
            "modelID":"m","providerID":"p","time":{"created":1},"sessionID":SID}}})
    }

    fn text_part(mid: &str, pid: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"message.part.updated","id":"evt","properties":{"sessionID":SID,"time":1,
            "part":{"type":"text","text":text,"messageID":mid,"sessionID":SID,"id":pid}}})
    }

    fn delta(mid: &str, pid: &str, d: &str) -> serde_json::Value {
        serde_json::json!({"type":"message.part.delta","id":"evt","properties":{"sessionID":SID,
            "messageID":mid,"partID":pid,"field":"text","delta":d}})
    }

    fn loaded_store() -> Store {
        let mut s = Store::default();
        s.reconcile_inner(SID, vec![]);
        s
    }

    fn text_of(s: &Store, mi: usize) -> String {
        let conv = &s.conversations[SID];
        part_text(&conv.messages[mi].parts[0]).unwrap().to_string()
    }

    #[test]
    fn streaming_deltas_append() {
        let mut s = loaded_store();
        assert_eq!(
            s.apply_inner(ev(assistant("msg_2"))),
            vec![StoreEvent::MessageInserted {
                session_id: SID.into(),
                index: 0
            }]
        );
        s.apply_inner(ev(text_part("msg_2", "prt_1", "")));
        for d in ["Hel", "lo ", "world"] {
            let ch = s.apply_inner(ev(delta("msg_2", "prt_1", d)));
            assert_eq!(
                ch,
                vec![StoreEvent::MessageChanged {
                    session_id: SID.into(),
                    index: 0
                }]
            );
        }
        assert_eq!(text_of(&s, 0), "Hello world");
        // Final part.updated carries the full text and self-heals.
        s.apply_inner(ev(text_part("msg_2", "prt_1", "Hello world!")));
        assert_eq!(text_of(&s, 0), "Hello world!");
    }

    #[test]
    fn events_for_unloaded_sessions_are_ignored() {
        let mut s = Store::default();
        assert!(s.apply_inner(ev(assistant("msg_2"))).is_empty());
        assert!(!s.conversations.contains_key(SID));
    }

    #[test]
    fn part_before_message_creates_placeholder() {
        let mut s = loaded_store();
        s.apply_inner(ev(text_part("msg_9", "prt_1", "early")));
        assert_eq!(text_of(&s, 0), "early");
        s.apply_inner(ev(assistant("msg_9")));
        assert_eq!(s.conversations[SID].messages.len(), 1);
    }

    #[test]
    fn messages_stay_sorted_by_id() {
        let mut s = loaded_store();
        s.apply_inner(ev(assistant("msg_3")));
        let ch = s.apply_inner(ev(assistant("msg_2")));
        assert_eq!(
            ch,
            vec![StoreEvent::MessageInserted {
                session_id: SID.into(),
                index: 0
            }]
        );
        let ids: Vec<_> = s.conversations[SID]
            .messages
            .iter()
            .map(|m| m.id().to_string())
            .collect();
        assert_eq!(ids, ["msg_2", "msg_3"]);
    }

    fn history(mid: &str, pid: &str, text: &str) -> Vec<oc::MessageWithParts> {
        let info = match ev(assistant(mid)) {
            Event::MessageUpserted { info, .. } => info,
            _ => unreachable!(),
        };
        let part = match ev(text_part(mid, pid, text)) {
            Event::PartUpserted { part, .. } => part,
            _ => unreachable!(),
        };
        vec![oc::MessageWithParts {
            info,
            parts: vec![part],
        }]
    }

    #[test]
    fn reconcile_keeps_local_text_that_extends_snapshot() {
        let mut s = loaded_store();
        s.apply_inner(ev(assistant("msg_2")));
        s.apply_inner(ev(text_part("msg_2", "prt_1", "Hello wor")));
        // Snapshot taken earlier than our latest delta.
        s.reconcile_inner(SID, history("msg_2", "prt_1", "Hello"));
        assert_eq!(text_of(&s, 0), "Hello wor");
    }

    #[test]
    fn reconcile_fills_in_missed_frames() {
        let mut s = loaded_store();
        s.apply_inner(ev(assistant("msg_2")));
        s.apply_inner(ev(text_part("msg_2", "prt_1", "Hel")));
        // We missed deltas while disconnected; the snapshot is ahead.
        s.reconcile_inner(SID, history("msg_2", "prt_1", "Hello world"));
        assert_eq!(text_of(&s, 0), "Hello world");
    }

    #[test]
    fn reconcile_keeps_messages_newer_than_snapshot() {
        let mut s = loaded_store();
        s.apply_inner(ev(assistant("msg_5")));
        s.reconcile_inner(SID, history("msg_2", "prt_1", "old"));
        let ids: Vec<_> = s.conversations[SID]
            .messages
            .iter()
            .map(|m| m.id().to_string())
            .collect();
        assert_eq!(ids, ["msg_2", "msg_5"]);
    }

    #[test]
    fn duplicate_session_errors_keep_first_line() {
        let mut s = loaded_store();
        for m in [
            "Model not found: x.",
            "ProviderModelNotFoundError: Model not found: x.\n    at foo",
        ] {
            s.apply_inner(ev(
                serde_json::json!({"type":"session.error","id":"e","properties":{
                "sessionID":SID,"error":{"name":"UnknownError","data":{"message":m}}}}),
            ));
        }
        assert_eq!(s.errors[SID], "Model not found: x.");
    }

    #[test]
    fn status_idle_clears_busy() {
        let mut s = Store::default();
        s.set_status_inner(SID, RunState::Busy);
        assert!(s.is_busy(SID));
        s.apply_inner(ev(
            serde_json::json!({"type":"session.idle","id":"e","properties":{"sessionID":SID}}),
        ));
        assert!(!s.is_busy(SID));
    }
}
