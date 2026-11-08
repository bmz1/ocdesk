//! Permission requests and question forms: snapshot reads guarded by the
//! store's revisions, and explicit reply/dismiss admission and completion.
//! The state itself lives in `store::Permissions` / `store::Forms`.

use super::*;

impl Controller {
    /// Pending permissions and question forms of every session in `dir`,
    /// in one request each (instead of two per session after every
    /// session-list refresh).
    pub(super) fn refresh_folder_pending(
        &mut self,
        dir: PathBuf,
        sessions: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api.clone() else { return };
        // (session, permission revision, form revision) at request time.
        let revisions: Vec<(String, Revision, Revision)> = self.store.update(cx, |s, _| {
            sessions
                .into_iter()
                .map(|sid| {
                    let (p, f) = (s.permissions.revision(&sid), s.forms.revision(&sid));
                    (sid, p, f)
                })
                .collect()
        });
        let d = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                let read = |forms: bool| {
                    let (api, d) = (api.clone(), d.clone());
                    with_location_retry(move || {
                        let (api, d) = (api.clone(), d.clone());
                        async move {
                            Ok(if forms {
                                (Vec::new(), api.folder_forms(&d).await?)
                            } else {
                                (api.folder_permissions(&d).await?, Vec::new())
                            })
                        }
                    })
                };
                let (p, f) = futures::join!(read(false), read(true));
                anyhow::Ok((p.map(|x| x.0), f.map(|x| x.1)))
            },
            move |this, res, cx| {
                let Ok((permissions, forms)) = res else {
                    return;
                };
                let mut stale_permissions = Vec::new();
                let mut stale_forms = Vec::new();
                let permissions_failed = permissions.as_ref().is_err_and(|e| !is_location_404(e));
                let forms_failed = forms.as_ref().is_err_and(|e| !is_location_404(e));
                this.store.update(cx, |s, cx| {
                    for (sid, p_rev, f_rev) in &revisions {
                        // A live event (or newer snapshot) wins over this
                        // read; a session removed meanwhile is not re-read.
                        let p_fresh = s.permissions.freshness(sid, *p_rev);
                        let f_fresh = s.forms.freshness(sid, *f_rev);
                        // A failed folder read falls back to per-session reads.
                        if (permissions.is_ok() && p_fresh == Freshness::Superseded)
                            || (p_fresh != Freshness::Forgotten && permissions_failed)
                        {
                            stale_permissions.push(sid.clone());
                        }
                        if (forms.is_ok() && f_fresh == Freshness::Superseded)
                            || (f_fresh != Freshness::Forgotten && forms_failed)
                        {
                            stale_forms.push(sid.clone());
                        }
                        if let Ok(all) = &permissions
                            && p_fresh == Freshness::Current
                        {
                            {
                                let mine: Vec<_> = all
                                    .iter()
                                    .filter(|r| &r.session_id == sid)
                                    .cloned()
                                    .collect();
                                // Untouched sessions are skipped: no 200 notifies per refresh.
                                if !mine.is_empty() || s.permissions.is_tracked(sid) {
                                    s.set_permissions(sid, mine, cx);
                                }
                            }
                        }
                        if let Ok(all) = &forms
                            && f_fresh == Freshness::Current
                        {
                            {
                                let mine: Vec<_> = all
                                    .iter()
                                    .filter(|f| &f.session_id == sid)
                                    .cloned()
                                    .collect();
                                if !mine.is_empty() || s.forms.is_tracked(sid) {
                                    s.set_forms(sid, mine, cx);
                                }
                            }
                        }
                    }
                });
                for e in [permissions.as_ref().err(), forms.as_ref().err()]
                    .into_iter()
                    .flatten()
                {
                    tracing::warn!(dir = %dir.display(), "folder approvals read failed: {e:#}");
                }
                for sid in stale_permissions {
                    this.refresh_permissions(sid, cx);
                }
                for sid in stale_forms {
                    this.refresh_forms(sid, cx);
                }
            },
        );
    }

    pub fn refresh_permissions(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.update(cx, |s, _| s.permissions.revision(&sid));
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    async move { api.permissions(&id).await }
                })
                .await
            },
            move |this, res, cx| {
                let freshness = this.store.read(cx).permissions.freshness(&sid, revision);
                if freshness == Freshness::Forgotten {
                    // Removed while in flight: success or failure, nothing to show.
                    tracing::debug!(op = "permissions", session_id = %sid, "dropped read of a removed session");
                    return;
                }
                match res {
                    Ok(requests) => {
                        // A live asked/replied event (or newer snapshot) wins over this read.
                        if freshness == Freshness::Current {
                            this.store
                                .update(cx, |s, cx| s.set_permissions(&sid, requests, cx));
                        } else {
                            this.refresh_permissions(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!(op = "permissions", session_id = %sid, error = %format!("{e:#}"), "location not ready; permission requests not loaded")
                    }
                    Err(e) => this.notice(format!("Could not load permission requests: {e:#}"), cx),
                }
            },
        );
    }

    pub fn refresh_forms(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.update(cx, |s, _| s.forms.revision(&sid));
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                with_location_retry(move || {
                    let api = api.clone();
                    let id = id.clone();
                    async move { api.forms(&id).await }
                })
                .await
            },
            move |this, res, cx| {
                let freshness = this.store.read(cx).forms.freshness(&sid, revision);
                if freshness == Freshness::Forgotten {
                    tracing::debug!(op = "forms", session_id = %sid, "dropped read of a removed session");
                    return;
                }
                match res {
                    Ok(forms) => {
                        // A live created/replied event (or newer snapshot) wins over this read.
                        if freshness == Freshness::Current {
                            this.store.update(cx, |s, cx| s.set_forms(&sid, forms, cx));
                        } else {
                            this.refresh_forms(sid, cx);
                        }
                    }
                    Err(e) if is_location_404(&e) => {
                        tracing::warn!(op = "forms", session_id = %sid, error = %format!("{e:#}"), "location not ready; question forms not loaded")
                    }
                    Err(e) => this.notice(format!("Could not load question forms: {e:#}"), cx),
                }
            },
        );
    }

    pub fn reply_form(
        &mut self,
        sid: String,
        fid: String,
        answer: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else { return };
        let (session, form) = (sid.clone(), fid.clone());
        self.send_form_reply(
            sid,
            fid,
            FormClose::Reply,
            async move { api.reply_form(&session, &form, &answer).await },
            cx,
        );
    }

    /// Record one answer in a form's draft (option picked, box toggled, text
    /// typed). Pure UI state; the server sees it only on submit.
    pub fn set_form_answer(
        &self,
        form_id: String,
        key: String,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |s, cx| {
            s.set_form_draft(&form_id, &key, value, cx);
        });
    }

    /// The committed draft answers of the selected form.
    pub fn form_draft(&self, form_id: String, cx: &Context<Self>) -> serde_json::Value {
        self.store.read(cx).forms.draft(&form_id)
    }

    pub fn cancel_form(&mut self, sid: String, fid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let (session, form) = (sid.clone(), fid.clone());
        self.send_form_reply(
            sid,
            fid,
            FormClose::Cancel,
            async move { api.cancel_form(&session, &form).await },
            cx,
        );
    }

    /// Reply to or dismiss a form. Refused while disconnected (its outcome
    /// could never report back: a disconnect abandons the "sending" state and
    /// this connection-scoped completion is then discarded) or while another
    /// reply to it is in flight. Success closes the form at once (the stream
    /// confirms it too); failure keeps it, with the error, retryable.
    fn send_form_reply(
        &mut self,
        sid: String,
        fid: String,
        close: FormClose,
        request: impl Future<Output = Result<()>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.store.read(cx).connection() != Connection::Connected {
            return;
        }
        let admitted = self.store.update(cx, |s, cx| {
            let admitted = s.forms.begin_reply(&sid, &fid);
            if admitted {
                cx.notify();
            }
            admitted
        });
        if !admitted {
            return;
        }
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            request,
            move |this, res, cx| {
                this.store.update(cx, |s, cx| match res {
                    Ok(()) => {
                        let (session_id, form_id) = (sid.clone(), fid.clone());
                        let event = match close {
                            FormClose::Reply => Event::FormReplied { session_id, form_id },
                            FormClose::Cancel => Event::FormCancelled { session_id, form_id },
                        };
                        s.apply(Envelope::bare(event), cx);
                    }
                    Err(e) => {
                        let msg = format!("{}: {e:#}", close.failure());
                        tracing::warn!(op = close.op(), session_id = %sid, form_id = %fid, error = %format!("{e:#}"), "form reply failed");
                        s.forms.reply_failed(&sid, &fid, msg);
                        cx.notify();
                    }
                });
                this.refresh_forms(sid, cx);
            },
        );
    }

    pub fn reply_permission(
        &mut self,
        sid: String,
        rid: String,
        decision: PermissionDecision,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else { return };
        // Same admission as forms: connected, and one reply in flight.
        if self.store.read(cx).connection() != Connection::Connected {
            return;
        }
        let admitted = self.store.update(cx, |s, cx| {
            let admitted = s.permissions.begin_reply(&sid, &rid);
            if admitted {
                cx.notify();
            }
            admitted
        });
        if !admitted {
            return;
        }
        let (session, request) = (sid.clone(), rid.clone());
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                api.reply_permission(&session, &request, decision, message.as_deref())
                    .await
            },
            move |this, res, cx| {
                let saved = res.is_ok() && decision == PermissionDecision::Always;
                this.store.update(cx, |s, cx| match res {
                    Ok(()) => s.apply(
                        Envelope::bare(Event::PermissionReplied {
                            session_id: sid.clone(),
                            request_id: rid,
                        }),
                        cx,
                    ),
                    Err(e) => {
                        tracing::warn!(op = "reply_permission", session_id = %sid, request_id = %rid, error = %format!("{e:#}"), "permission reply failed");
                        s.permissions
                            .reply_failed(&sid, &rid, format!("Could not reply: {e:#}"));
                        cx.notify();
                    }
                });
                this.refresh_permissions(sid, cx);
                if saved {
                    this.refresh_saved_grants(cx);
                }
            },
        );
    }
}

/// How [`Controller::send_form_reply`] closes a form.
#[derive(Clone, Copy)]
enum FormClose {
    Reply,
    Cancel,
}

impl FormClose {
    fn op(self) -> &'static str {
        match self {
            Self::Reply => "reply_form",
            Self::Cancel => "cancel_form",
        }
    }

    fn failure(self) -> &'static str {
        match self {
            Self::Reply => "Could not reply",
            Self::Cancel => "Could not cancel",
        }
    }
}
