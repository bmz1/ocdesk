//! Session inboxes (queued/steering prompts), subagent sessions and shells.

use super::*;

impl Store {
    /// The M7 events. A delivered user item is rewritten into a
    /// `UserPrompt` and continues to the transcript code.
    pub(super) fn apply_m7(&mut self, env: &mut Envelope) -> M7 {
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
                if let Some(dir) = &env.directory {
                    self.shell_snapshots
                        .entry(dir.clone())
                        .or_default()
                        .insert(info.id.clone());
                }
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

    /// Replace only this folder's shell snapshot, including removed shells.
    pub fn set_shells(&mut self, dir: &Path, shells: Vec<ShellInfo>, cx: &mut Context<Self>) {
        self.replace_shells(dir, shells);
        cx.emit(StoreEvent::TasksChanged);
        cx.notify();
    }

    pub(super) fn replace_shells(&mut self, dir: &Path, shells: Vec<ShellInfo>) {
        let ids: HashSet<_> = shells.iter().map(|sh| sh.id.clone()).collect();
        let previous = self
            .shell_snapshots
            .insert(dir.to_path_buf(), ids.clone())
            .unwrap_or_default();
        let removed: HashSet<_> = self
            .shells
            .iter()
            .filter(|(id, sh)| {
                !ids.contains(*id)
                    && (previous.contains(*id)
                        || sh
                            .session_id
                            .as_deref()
                            .and_then(|sid| self.session(sid))
                            .is_some_and(|s| s.directory == dir))
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.shells.retain(|id, _| !removed.contains(id));
        for sh in shells {
            self.shells.insert(sh.id.clone(), sh);
        }
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
}
