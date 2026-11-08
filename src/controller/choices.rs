//! Model and agent choices: shown at once, confirmed by the stream
//! (`session.model.selected` / `session.agent.selected`). See
//! `domain::choice` for what a late or failed completion may still change.

use super::*;

impl Controller {
    /// Pick the model (and variant) for the selected session, or for the
    /// next new session when nothing is selected.
    pub fn choose_model(&mut self, model: ModelRef, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.store
                .update(cx, |s, cx| s.set_draft_choice(Some(model), None, cx));
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let token = self.show_model(&sid, &model, cx);
        let target = sid.clone();
        let chosen = model.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.switch_model(&sid, &model).await },
            move |this, res, cx| {
                this.settle_model(target, token, chosen, res.is_err(), cx);
                if let Err(e) = res {
                    this.notice(format!("Could not switch model: {e:#}"), cx);
                }
            },
        );
    }

    /// Pick the primary agent for the selected session, or the next new one.
    pub fn choose_agent(&mut self, agent: String, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.store
                .update(cx, |s, cx| s.set_draft_choice(None, Some(agent), cx));
            return;
        };
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let token = self.show_agent(&sid, &agent, cx);
        let target = sid.clone();
        let chosen = agent.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.switch_agent(&sid, &agent).await },
            move |this, res, cx| {
                this.settle_agent(target, token, chosen, res.is_err(), cx);
                if let Err(e) = res {
                    this.notice(format!("Could not switch agent: {e:#}"), cx);
                }
            },
        );
    }

    fn show_model(&mut self, sid: &str, model: &ModelRef, cx: &mut Context<Self>) -> ChoiceToken {
        let shown = self
            .store
            .read(cx)
            .session(sid)
            .and_then(|s| s.model.clone());
        let token = self.model_choices.choose(sid, shown);
        self.show_selection(
            Event::ModelSelected {
                session_id: sid.to_owned(),
                model: model.clone(),
            },
            cx,
        );
        token
    }

    fn settle_model(
        &mut self,
        sid: String,
        token: ChoiceToken,
        model: ModelRef,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if !failed {
            self.model_choices.succeeded(&sid, token, model);
        } else if let Some(model) = self.model_choices.failed(&sid, token) {
            tracing::debug!(session_id = %sid, "rolling back failed model choice");
            self.show_selection(
                Event::ModelSelected {
                    session_id: sid,
                    model,
                },
                cx,
            );
        }
    }

    pub(super) fn show_agent(
        &mut self,
        sid: &str,
        agent: &str,
        cx: &mut Context<Self>,
    ) -> ChoiceToken {
        let shown = self
            .store
            .read(cx)
            .session(sid)
            .and_then(|s| s.agent.clone());
        let token = self.agent_choices.choose(sid, shown);
        self.show_selection(
            Event::AgentSelected {
                session_id: sid.to_owned(),
                agent: agent.to_owned(),
            },
            cx,
        );
        token
    }

    pub(super) fn settle_agent(
        &mut self,
        sid: String,
        token: ChoiceToken,
        agent: String,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if !failed {
            self.agent_choices.succeeded(&sid, token, agent);
        } else if let Some(agent) = self.agent_choices.failed(&sid, token) {
            tracing::debug!(session_id = %sid, "rolling back failed agent choice");
            self.show_selection(
                Event::AgentSelected {
                    session_id: sid,
                    agent,
                },
                cx,
            );
        }
    }

    /// Apply a locally decided selection; it bypasses [`Self::on_event`], so
    /// the optimistic trackers don't mistake it for a server report.
    fn show_selection(&mut self, event: Event, cx: &mut Context<Self>) {
        self.store
            .update(cx, |s, cx| s.apply(Envelope::bare(event), cx));
    }
}
