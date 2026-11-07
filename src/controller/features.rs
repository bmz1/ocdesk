use super::*;
use crate::api::features::{ReviewMode, patch_fingerprint};
use crate::settings::PermissionMode;
use gpui_kit::App;

impl Controller {
    pub fn preferences(&self) -> Settings {
        self.settings.clone()
    }
    pub fn show_notice(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.notice(message, cx);
    }
    pub fn save_preferences(&mut self, prefs: Settings, cx: &mut Context<Self>) {
        // Only screen-editable fields: retain live window, read state and review marks.
        self.update_settings(cx, |s| {
            s.theme = prefs.theme;
            s.permission_mode = prefs.permission_mode;
            s.project_defaults = prefs.project_defaults;
            s.keybindings = prefs.keybindings;
            s.editor = prefs.editor;
            s.notifications = prefs.notifications;
            s.draft_model = prefs.draft_model;
            s.draft_agent = prefs.draft_agent;
        });
        let folder = self.store.read(cx).target_folder();
        if self.store.read(cx).selected.is_none() {
            self.apply_draft_defaults(folder.as_deref(), cx);
        }
    }
    pub(super) fn apply_draft_defaults(
        &mut self,
        folder: Option<&std::path::Path>,
        cx: &mut Context<Self>,
    ) {
        let p = folder.and_then(|f| self.settings.project_defaults.get(f));
        let model = p
            .and_then(|p| p.model.clone())
            .or_else(|| self.settings.draft_model.clone());
        let agent = p
            .and_then(|p| p.agent.clone())
            .or_else(|| self.settings.draft_agent.clone());
        self.store.update(cx, |s, cx| {
            s.draft_permission = None;
            s.draft_model = model;
            s.draft_agent = agent;
            cx.notify();
        });
    }
    pub fn refresh_session_mode(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let generation = self.generation;
        let version = *self.mode_requests.entry(sid.clone()).or_default();
        self.run(
            cx,
            async move { api.session_mode(&sid).await.map(|m| (sid, m)) },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                if let Ok((sid, m)) = res
                    && this.mode_requests.get(&sid) == Some(&version)
                {
                    this.store.update(cx, |s, cx| {
                        s.session_modes.insert(sid, m);
                        cx.notify();
                    });
                }
            },
        );
    }
    pub fn set_permission_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            self.store.update(cx, |s, cx| {
                s.draft_permission = Some(mode);
                cx.notify();
            });
            return;
        };
        if self.store.read(cx).mode_sending.contains(&sid) {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        *self.mode_requests.entry(sid.clone()).or_default() += 1;
        let generation = self.generation;
        self.store.update(cx, |s, cx| {
            s.mode_sending.insert(sid.clone());
            cx.notify();
        });
        let id = sid.clone();
        self.run(
            cx,
            async move { api.set_session_mode(&id, mode).await },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.mode_sending.remove(&sid);
                    if res.is_ok() {
                        s.session_modes.insert(sid, Some(mode));
                    }
                    cx.notify();
                });
                if let Err(e) = res {
                    this.notice(format!("Could not change permissions: {e:#}"), cx);
                }
            },
        );
    }
    pub fn refresh_usage(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let Some(folder) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        // Coalesce stream bursts. A newer cumulative total invalidates the old REST response.
        if self.store.read(cx).usage_loading.contains(&sid) {
            return;
        }
        let generation = self.generation;
        let revision = *self.usage_revision.entry(sid.clone()).or_default();
        self.store.update(cx, |s, cx| {
            s.usage_loading.insert(sid.clone());
            s.usage_errors.remove(&sid);
            cx.notify();
        });
        let id = sid.clone();
        self.run(
            cx,
            async move { api.usage_report(&id, &folder).await },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                let changed = this.usage_revision.get(&sid) != Some(&revision);
                this.store.update(cx, |s, cx| {
                    s.usage_loading.remove(&sid);
                    match res {
                        Ok(mut report) => {
                            if changed && let Some(live) = s.usage.get(&sid) {
                                report.tokens = live.tokens.clone();
                                report.cost = live.cost;
                            }
                            s.usage.insert(sid.clone(), report);
                        }
                        Err(e) => {
                            s.usage_errors.insert(sid.clone(), format!("{e:#}"));
                        }
                    }
                    cx.notify();
                });
                if changed {
                    this.refresh_usage(sid, cx);
                }
            },
        );
    }
    pub fn set_review(&mut self, mode: ReviewMode, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        self.store.update(cx, |s, cx| {
            s.review.insert(sid.clone(), mode);
            s.diffs.remove(&sid);
            cx.notify();
        });
        self.refresh_diff(sid, cx);
    }
    pub fn load_commits(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        let Some(dir) = self.store.read(cx).target_folder() else {
            return;
        };
        let generation = self.generation;
        self.run(
            cx,
            async move { crate::api::features::commits(&dir).await },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                match res {
                    Ok(commits) => this.store.update(cx, |s, cx| {
                        s.commits.insert(sid, commits);
                        cx.notify();
                    }),
                    Err(e) => this.notice(format!("Could not load commits: {e:#}"), cx),
                }
            },
        );
    }
    pub fn viewed_key(&self, sid: &str, file: &str, cx: &App) -> String {
        let s = self.store.read(cx);
        let dir = s
            .session(sid)
            .map(|s| s.directory.display().to_string())
            .unwrap_or_default();
        let mode = s.review.get(sid).cloned().unwrap_or_default();
        let base = s.review_base.get(sid).cloned().unwrap_or_default();
        // Turn views are session-specific; branch/commit views can survive reopening threads.
        format!(
            "{dir}\n{}\n{file}",
            if mode == ReviewMode::Turn {
                format!("turn:{sid}")
            } else {
                format!("{}:{base}", mode.key())
            }
        )
    }
    pub fn mark_viewed(
        &mut self,
        sid: String,
        file: String,
        patch: String,
        cx: &mut Context<Self>,
    ) {
        let key = self.viewed_key(&sid, &file, cx);
        let hash = patch_fingerprint(&patch);
        self.update_settings(cx, |s| {
            if s.viewed.get(&key) == Some(&hash) {
                s.viewed.remove(&key);
            } else {
                s.viewed.insert(key, hash);
            }
        });
    }
    pub fn open_file(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(dir) = self.store.read(cx).target_folder() else {
            return;
        };
        let editor = self
            .settings
            .editor
            .clone()
            .unwrap_or_else(|| "code".into());
        self.run(
            cx,
            async move {
                let dir = tokio::fs::canonicalize(dir).await?;
                let path = tokio::fs::canonicalize(dir.join(file)).await?;
                if !path.starts_with(&dir) {
                    anyhow::bail!("file is outside the project");
                }
                let mut child = tokio::process::Command::new(editor)
                    .arg("--")
                    .arg(path)
                    .spawn()?;
                // Reap in background; opening an editor must not block the UI.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                Ok(())
            },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not open editor: {e:#}"), cx)
                }
            },
        );
    }
    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.window_active = active;
            cx.notify();
        });
        if active {
            self.mark_selected_read(cx);
        }
    }
    pub(super) fn mark_selected_read(&mut self, cx: &mut Context<Self>) {
        let s = self.store.read(cx);
        if !s.window_active {
            return;
        }
        let Some(sid) = s.selected.clone() else {
            return;
        };
        if !s.unread.contains(&sid) {
            return;
        }
        self.store.update(cx, |s, cx| {
            s.unread.remove(&sid);
            cx.notify();
        });
        cx.dismiss_system_notification(&format!("ocdesk:{sid}"));
        self.update_settings(cx, |s| {
            s.unread.remove(&sid);
        });
    }
    pub(super) fn product_event(&mut self, env: &Envelope, cx: &mut Context<Self>) {
        if let Event::SessionRemoved { session_id } = &env.event {
            self.store.update(cx, |s, cx| {
                s.unread.remove(session_id);
                s.usage.remove(session_id);
                s.session_modes.remove(session_id);
                s.commits.remove(session_id);
                cx.notify();
            });
            self.update_settings(cx, |s| {
                s.unread.remove(session_id);
            });
            cx.dismiss_system_notification(&format!("ocdesk:{session_id}"));
            return;
        }
        if let Event::UsageUpdated {
            session_id,
            tokens,
            cost,
        } = &env.event
        {
            *self.usage_revision.entry(session_id.clone()).or_default() += 1;
            self.store.update(cx, |s, cx| {
                let r = s.usage.entry(session_id.clone()).or_default();
                r.tokens = crate::api::features::Tokens::parse(tokens);
                r.cost = *cost;
                cx.notify();
            });
        }
        let Some(attention) = crate::notifications::attention(&env.event, self.store.read(cx))
        else {
            return;
        };
        let key = format!("{}:{:?}", attention.key, env.seq);
        if (env.seq.is_some()
            || matches!(&env.event, Event::PermissionAsked(_) | Event::FormAsked(_)))
            && !self.attention_seen.insert(key)
        {
            return;
        }
        let root = attention.session;
        let status = attention.status;
        let title = attention.title;
        self.store.update(cx, |s, cx| {
            s.unread.insert(root.clone());
            cx.notify();
        });
        self.update_settings(cx, |s| {
            s.unread.insert(root.clone());
        });
        if self.settings.notifications {
            cx.show_system_notification(gpui_kit::SystemNotification {
                tag: format!("ocdesk:{root}").into(),
                title: status.into(),
                body: title.into(),
                actions: Vec::new(),
            });
        }
    }
}
impl Controller {
    pub fn view_store(&self) -> Entity<Store> {
        self.store.clone()
    }
    pub fn refresh_saved_grants(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let generation = self.generation;
        self.grant_requests += 1;
        let request = self.grant_requests;
        self.store.update(cx, |s, cx| {
            s.saved_grants_loading = true;
            s.saved_grants_error = None;
            cx.notify();
        });
        self.run(
            cx,
            async move { api.saved_grants().await },
            move |this, res, cx| {
                if this.generation != generation || this.grant_requests != request {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.saved_grants_loading = false;
                    match res {
                        Ok(grants) => s.saved_grants = grants,
                        Err(e) => s.saved_grants_error = Some(format!("{e:#}")),
                    }
                    cx.notify();
                });
            },
        );
    }
    pub fn revoke_saved_grant(&mut self, id: String, cx: &mut Context<Self>) {
        if self.store.read(cx).grant_sending.contains(&id) {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let generation = self.generation;
        self.grant_requests += 1;
        self.store.update(cx, |s, cx| {
            s.saved_grants_loading = false;
            s.grant_sending.insert(id.clone());
            cx.notify();
        });
        let target = id.clone();
        self.run(
            cx,
            async move { api.revoke_grant(&target).await },
            move |this, res, cx| {
                if this.generation != generation {
                    return;
                }
                this.grant_requests += 1;
                this.store.update(cx, |s, cx| {
                    s.grant_sending.remove(&id);
                    if res.is_ok() {
                        s.saved_grants.retain(|g| g.id != id);
                    }
                    cx.notify();
                });
                if let Err(e) = res {
                    this.notice(format!("Could not revoke saved permission: {e:#}"), cx);
                }
                this.refresh_saved_grants(cx);
            },
        );
    }
}

impl Controller {
    pub fn set_review_base(&mut self, sid: String, base: String, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            if base.trim().is_empty() {
                s.review_base.remove(&sid);
            } else {
                s.review_base.insert(sid.clone(), base.trim().to_string());
            }
            s.diffs.remove(&sid);
            cx.notify();
        });
        self.refresh_diff(sid, cx);
    }
}
