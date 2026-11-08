//! Prompt and command admission (creating the session on first prompt),
//! the inbox of queued/steering prompts, and interrupting a turn.

use super::*;

impl Controller {
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

    pub fn has_failed_submission(&self) -> bool {
        self.failed_submission.is_some()
    }

    pub(super) fn submit(&mut self, what: Submission, cx: &mut Context<Self>) {
        self.submit_to(what, None, cx);
    }

    pub(super) fn submit_to(
        &mut self,
        what: Submission,
        target: Option<(Option<String>, PathBuf)>,
        cx: &mut Context<Self>,
    ) {
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
        let selected = target
            .as_ref()
            .map(|t| t.0.clone())
            .unwrap_or_else(|| store.selected.clone());
        let original_selection = store.selected.clone();
        let (draft_model, draft_agent) = (store.draft_model.clone(), store.draft_agent.clone());
        let Some(folder) = target.map(|t| t.1).or_else(|| store.target_folder()) else {
            self.notice("Add a project first", cx);
            return;
        };
        let permission_mode = store.draft_permission;
        let recovery = (what.clone(), selected.clone(), folder.clone());
        self.submission_seq += 1;
        let submission = self.submission_seq;
        let submission_target = selected
            .clone()
            .map(SubmissionTarget::Session)
            .unwrap_or_else(|| SubmissionTarget::Draft(folder.clone()));
        self.store.update(cx, |s, cx| {
            s.submitting.insert(submission, submission_target.clone());
            if let SubmissionTarget::Session(sid) = &submission_target {
                cx.emit(crate::store::StoreEvent::StatusChanged {
                    session_id: sid.clone(),
                });
            }
            cx.notify();
        });
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (sid, created) = match selected {
                    Some(sid) => (sid, None),
                    None => {
                        let mut s = api.create_session(&folder).await?;
                        if let Some(mode) = permission_mode {
                            api.set_session_mode(&s.id, mode).await?;
                        }
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
                this.store.update(cx, |s, cx| {
                    s.submitting.remove(&submission);
                    if let SubmissionTarget::Session(sid) = &submission_target {
                        cx.emit(crate::store::StoreEvent::StatusChanged { session_id: sid.clone() });
                    }
                    cx.notify();
                });
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

    /// Re-read a session's pending inbox (select, reconnect).
    pub fn refresh_inbox(&mut self, sid: String, cx: &mut Context<Self>) {
        self.read_inbox(sid, 0, cx);
    }

    fn read_inbox(&mut self, sid: String, attempt: u32, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let target = sid.clone();
        let revision = self
            .store
            .update(cx, |s, _| s.inbox_revision.current(sid.clone()));
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.inbox(&sid).await },
            move |this, res, cx| {
                if !this.store.read(cx).inbox_revision.is_current(&target, revision) {
                    let again = target.clone();
                    if this.retry_snapshot(attempt, cx, move |this, cx| {
                        this.read_inbox(again, attempt + 1, cx)
                    }) {
                        return;
                    }
                }
                match res {
                    Ok(items) => this
                        .store
                        .update(cx, |s, cx| s.set_inbox(&target, items, cx)),
                    Err(e) => tracing::warn!(op = "inbox", session_id = %target, error = %format!("{e:#}"), "could not read inbox"),
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
        if self.store.read(cx).interrupting.contains(&sid) {
            return;
        }
        let session = sid.clone();
        self.store.update(cx, |s, cx| {
            s.interrupting.insert(sid.clone());
            cx.emit(crate::store::StoreEvent::StatusChanged {
                session_id: sid.clone(),
            });
            cx.notify();
        });
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.interrupt(&sid).await },
            move |this, res, cx| {
                if let Err(e) = res {
                    this.store.update(cx, |s, cx| {
                        s.interrupting.remove(&session);
                        cx.emit(crate::store::StoreEvent::StatusChanged {
                            session_id: session,
                        });
                        cx.notify();
                    });
                    this.notice(format!("Could not stop: {e:#}"), cx);
                } else {
                    this.refresh_active(cx);
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

/// What the composer sends: a prompt or a server command.
#[derive(Clone)]
pub(super) enum Submission {
    Prompt(PromptInput),
    Command {
        name: String,
        text: String,
        delivery: Option<crate::api::model::Delivery>,
    },
}
