//! Session list, history loading, selection and per-session operations
//! (rename, delete, compact, fork, undo, implement plan).

use super::*;

impl Controller {
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
}
