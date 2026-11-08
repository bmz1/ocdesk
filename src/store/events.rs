//! Folding normalized server events into the store, and turning the
//! resulting changes into fine-grained `StoreEvent`s.

use super::*;

impl Store {
    /// Fold one event. Emits [`StoreEvent`]s and notifies when anything changed.
    pub fn apply(&mut self, env: Envelope, cx: &mut Context<Self>) {
        let events = self.apply_inner(env);
        self.emit_all(events, cx);
    }

    pub(crate) fn apply_inner(&mut self, mut env: Envelope) -> Vec<StoreEvent> {
        if matches!(
            env.event,
            Event::SessionUpserted(_)
                | Event::SessionRemoved { .. }
                | Event::SessionRenamed { .. }
                | Event::ShellCreated(_)
                | Event::ShellExited { .. }
                | Event::ShellDeleted { .. }
        ) {
            self.tasks_revision += 1;
        }

        if matches!(
            env.event,
            Event::ExecutionStarted { .. }
                | Event::ExecutionEnded { .. }
                | Event::SessionUpserted(_)
                | Event::SessionRemoved { .. }
                | Event::SessionRenamed { .. }
                | Event::ModelSelected { .. }
                | Event::AgentSelected { .. }
                | Event::InboxEnqueued { .. }
                | Event::InboxDelivered { .. }
                | Event::InboxCancelled { .. }
                | Event::InboxDeliveryChanged { .. }
        ) {
            self.snapshot_revision += 1;
        }
        if let Event::InboxEnqueued { session_id, .. }
        | Event::InboxDelivered { session_id, .. }
        | Event::InboxCancelled { session_id, .. }
        | Event::InboxDeliveryChanged { session_id, .. } = &env.event
        {
            self.inbox_revision.bump(session_id.clone());
        }
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
                self.forget_session(session_id);
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
                self.permissions.asked(request.clone());
                return vec![StoreEvent::StatusChanged { session_id: sid }];
            }
            Event::PermissionReplied {
                session_id,
                request_id,
            } => {
                self.permissions.replied(session_id, request_id);
                return vec![StoreEvent::StatusChanged {
                    session_id: session_id.clone(),
                }];
            }
            Event::FormAsked(form) => {
                let sid = form.session_id.clone();
                self.forms.asked(form.clone());
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
                self.forms.closed(session_id, form_id);
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
        if !conv.load.loaded() {
            // Replayed onto the snapshot when it lands.
            if conv.load.fetching() {
                if conv.pending.len() >= MAX_PENDING {
                    conv.cancel_load();
                    self.needs_reload.insert(session_id.clone());
                    self.load_errors.insert(
                        session_id,
                        "History changed too quickly; retry loading.".into(),
                    );
                } else {
                    conv.pending.push(env);
                }
            }
            return out;
        }
        // Retain terminal mutations received after the fetch boundary. Do not
        // replay earlier running/progress states over a newer terminal snapshot.
        if conv.load.fetching()
            && matches!(
                env.event,
                Event::ToolSucceeded { .. }
                    | Event::ToolFailed { .. }
                    | Event::StepEnded { .. }
                    | Event::StepFailed { .. }
                    | Event::BlockEnded { .. }
            )
        {
            if conv.pending.len() >= MAX_PENDING {
                conv.cancel_load();
                self.needs_reload.insert(session_id.clone());
            } else {
                conv.pending.push(env.clone());
            }
        }
        if conv.load.fetching()
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
        let text_delta = matches!(env.event, Event::BlockDelta { .. });
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

        // A server bug (or id collision) must not crash the app: drop the
        // event and refetch the session's history once its turn ends.
        if !matches!(env.event, Event::UserPrompt { .. })
            && let Some(id) = event_message(&env.event)
            && conv
                .index_of(id)
                .is_ok_and(|i| !matches!(conv.messages[i], Message::Assistant(_)))
        {
            tracing::warn!(
                session = %session_id,
                message = id,
                "assistant event for a non-assistant message; will reload"
            );
            self.needs_reload.insert(session_id.clone());
            return out;
        }
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
        if !out.is_empty() && !text_delta {
            conv.revision += 1;
        }
        out
    }

    pub(super) fn emit_all(&mut self, events: Vec<StoreEvent>, cx: &mut Context<Self>) {
        if events.is_empty() {
            return;
        }
        // A streamed delta only changes one row. Subscribers that show rows
        // (conversation, tasks) react to the event itself; skipping the
        // notify keeps every whole-store observer from redrawing per token.
        let content_only = events
            .iter()
            .all(|e| matches!(e, StoreEvent::MessageChanged { .. }));
        for e in events {
            cx.emit(e);
        }
        if !content_only {
            cx.notify();
        }
    }

    fn set_busy_inner(&mut self, session_id: &str, busy: bool) -> Vec<StoreEvent> {
        let changed = if busy {
            self.busy.insert(session_id.to_string())
        } else {
            self.busy.remove(session_id)
        };
        if !busy {
            self.retry.remove(session_id);
            self.interrupting.remove(session_id);
        }
        if changed {
            vec![StoreEvent::StatusChanged {
                session_id: session_id.to_string(),
            }]
        } else {
            vec![]
        }
    }
}

/// Append `deltas` to `text`, skipping the prefix of `deltas` that `text`
/// already ends with (the part the snapshot captured).
pub(super) fn append_overlapping(text: &mut String, deltas: &str) {
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

pub(super) fn prose_mut(
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
