//! The session list, selection and transcript retention, execution status,
//! session removal and usage summaries.

use super::*;

impl Store {
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

    pub fn is_submitting(&self) -> bool {
        let target = match &self.selected {
            Some(sid) => SubmissionTarget::Session(sid.clone()),
            None => match self.target_folder() {
                Some(folder) => SubmissionTarget::Draft(folder),
                None => return false,
            },
        };
        self.submitting.values().any(|t| *t == target)
    }

    /// Activity stays beside the latest message throughout server/model latency.
    pub fn response_activity(&self) -> Option<&'static str> {
        if self.is_submitting() {
            return Some("Sending…");
        }
        let sid = self.selected.as_ref()?;
        if self.interrupting.contains(sid) {
            return Some("Stopping…");
        }
        if !self.is_busy(sid) || self.retry.contains_key(sid) {
            return None;
        }
        // The approval/question cards already explain what the user must do.
        if std::iter::once(sid.as_str())
            .chain(self.children_of(sid).iter().map(|s| s.id.as_str()))
            .any(|id| self.permissions.has_pending(id) || self.forms.has_pending(id))
        {
            return None;
        }
        let latest = self.selected_conversation().and_then(|c| c.messages.last());
        Some(match latest {
            Some(Message::Assistant(a))
                if a.blocks.iter().any(|b| match b {
                    Block::Prose { text, .. } => !text.trim().is_empty(),
                    Block::Tool(_) => true,
                }) =>
            {
                "Working…"
            }
            _ => "Thinking…",
        })
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

    /// Drop everything kept about a deleted session (and its subagents).
    /// Every per-session map must be cleared here, or it leaks.
    pub(crate) fn forget_session(&mut self, sid: &str) {
        let kids: Vec<String> = self
            .children
            .remove(sid)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.id)
            .collect();
        for kid in &kids {
            self.forget_session(kid);
        }
        self.sessions.retain(|x| x.id != sid);
        for siblings in self.children.values_mut() {
            siblings.retain(|c| c.id != sid);
        }
        self.conversations.remove(sid);
        self.recent.retain(|r| r != sid);
        self.busy.remove(sid);
        self.retry.remove(sid);
        self.errors.remove(sid);
        self.load_errors.remove(sid);
        self.needs_reload.remove(sid);
        self.interrupting.remove(sid);
        self.submitting
            .retain(|_, target| *target != SubmissionTarget::Session(sid.to_string()));
        self.forms.forget_session(sid);
        self.permissions.forget_session(sid);
        self.inbox.remove(sid);
        self.inbox_revision.forget(&sid.to_string());
        self.session_modes.remove(sid);
        self.mode_sending.remove(sid);
        self.usage.remove(sid);
        self.usage_status.remove(sid);
        self.unread.remove(sid);
    }

    /// The name of a tool call, if its message is in memory.
    pub fn tool_name(&self, session_id: &str, message_id: &str, call_id: &str) -> Option<&str> {
        let conv = self.conversations.get(session_id)?;
        let Message::Assistant(a) = conv.messages.get(conv.index_of(message_id).ok()?)? else {
            return None;
        };
        a.blocks.iter().find_map(|b| match b {
            Block::Tool(t) if t.call_id == call_id && !t.name.is_empty() => Some(t.name.as_str()),
            _ => None,
        })
    }

    /// The loaded conversation's assistant messages that reported usage.
    fn usage_messages<'a>(
        &'a self,
        sid: &str,
    ) -> Option<impl DoubleEndedIterator<Item = &'a crate::api::model::AssistantMessage>> {
        let conv = self.conversations.get(sid).filter(|c| c.load.loaded())?;
        Some(conv.messages.iter().filter_map(|m| match m {
            Message::Assistant(a) if a.meta.tokens.is_some() => Some(a),
            _ => None,
        }))
    }

    /// Context used by the last request and the model's window. Derived
    /// from the loaded conversation and catalog, else the last REST report.
    pub fn usage_context(&self, sid: &str) -> (Option<u64>, Option<u64>) {
        let report = self.usage.get(sid);
        let last = self.usage_messages(sid).and_then(|mut it| it.next_back());
        let Some(last) = last else {
            return (
                report.and_then(|r| r.context),
                report.and_then(|r| r.context_limit),
            );
        };
        let context = last.meta.tokens.as_ref().map(|t| t.context());
        let session = self.session(sid);
        let model = last
            .meta
            .model
            .as_ref()
            .or_else(|| session.and_then(|s| s.model.as_ref()));
        let limit = session
            .and_then(|s| self.catalogs.get(&s.directory))
            .zip(model)
            .and_then(|(c, m)| {
                c.models
                    .iter()
                    .find(|x| x.provider_id == m.provider_id && x.id == m.id)
            })
            .and_then(|m| m.context_limit)
            .or_else(|| report.and_then(|r| r.context_limit));
        (context, limit)
    }

    /// The usage report, with the per-model breakdown and context computed
    /// from the loaded conversation (so the server needn't resend history).
    pub fn usage_view(&self, sid: &str) -> Option<crate::api::features::UsageReport> {
        let mut report = self.usage.get(sid)?.clone();
        if let Some(messages) = self.usage_messages(sid) {
            report.models.clear();
            for a in messages {
                let key = a
                    .meta
                    .model
                    .as_ref()
                    .map(|m| format!("{}/{}", m.provider_id, m.id))
                    .unwrap_or_else(|| "unknown/unknown".into());
                if let Some(t) = &a.meta.tokens {
                    report.add_model_usage(&key, t, a.meta.cost.unwrap_or(0.));
                }
            }
        }
        (report.context, report.context_limit) = self.usage_context(sid);
        Some(report)
    }

    /// Whether usage details must come from the server's history.
    pub fn usage_needs_history(&self, sid: &str) -> bool {
        !self.conversations.get(sid).is_some_and(|c| c.load.loaded())
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
                .is_none_or(|c| !c.load.loaded() && c.messages.is_empty())
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected.as_ref()?)
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
            if let Some(id) = &self.selected {
                self.recent.retain(|r| r != id);
                self.recent.push_back(id.clone());
                if self.recent.len() > KEEP_CONVERSATIONS {
                    self.recent.pop_front();
                }
            }
            self.evict_conversations();
            cx.emit(StoreEvent::SelectionChanged);
            cx.notify();
        }
    }

    /// Drop transcripts nobody looked at recently; selecting a session
    /// fetches its history again anyway. Anything still live is kept.
    pub(super) fn evict_conversations(&mut self) {
        if self.conversations.len() <= KEEP_CONVERSATIONS {
            return;
        }
        let keep_children: HashSet<&str> = self
            .recent
            .iter()
            .flat_map(|p| self.children.get(p).into_iter().flatten())
            .map(|c| c.id.as_str())
            .collect();
        let keep: HashSet<String> = self
            .conversations
            .iter()
            .filter(|(id, conv)| {
                self.recent.contains(id)
                    || keep_children.contains(id.as_str())
                    || conv.load.fetching()
                    || self.busy.contains(*id)
                    || self.permissions.has_pending(id)
                    || self.forms.has_pending(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.conversations.retain(|id, _| keep.contains(id));
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
        self.interrupting.retain(|sid| new.contains(sid));
        let changed: Vec<String> = self.busy.symmetric_difference(&new).cloned().collect();
        self.busy = new;
        let events = changed
            .into_iter()
            .map(|session_id| StoreEvent::StatusChanged { session_id })
            .collect();
        self.emit_all(events, cx);
    }
}
