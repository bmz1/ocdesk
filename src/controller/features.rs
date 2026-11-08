use super::*;
use crate::settings::PermissionMode;
use gpui_kit::App;

impl Controller {
    pub fn settings_mcp_servers(
        &self,
        directory: PathBuf,
        change: Option<(String, bool)>,
    ) -> Result<oneshot::Receiver<Result<Vec<crate::api::model::McpServer>>>> {
        let api = self
            .api
            .clone()
            .ok_or_else(|| anyhow!("OpenCode is not connected"))?;
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let result = async {
                if let Some((name, connect)) = change {
                    api.mcp_set_connected(&directory, &name, connect).await?;
                }
                api.mcp_servers(&directory).await
            }
            .await;
            let _ = tx.send(result);
        });
        Ok(rx)
    }

    pub fn reload_configuration(&self, cx: &App) -> Result<oneshot::Receiver<Result<()>>> {
        let store = self.store.read(cx);
        if !store.busy.is_empty()
            || !store.submitting.is_empty()
            || store.permissions.values().any(|r| !r.is_empty())
            || store.forms.values().any(|r| !r.is_empty())
        {
            anyhow::bail!(
                "Finish running turns and pending approvals before reloading configuration."
            );
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| anyhow!("OpenCode is not connected"))?;
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(api.reload_configuration().await);
        });
        Ok(rx)
    }

    pub fn configuration_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_catalogs(cx);
        if let Some(sid) = self.store.read(cx).selected.clone() {
            self.refresh_session_mode(sid, cx);
        }
    }
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
            s.keybindings = prefs.keybindings;
            s.editor = prefs.editor;
            s.notifications = prefs.notifications;
        });
    }
    pub(super) fn reset_draft_choices(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.draft_permission = None;
            s.draft_model = None;
            s.draft_agent = None;
            cx.notify();
        });
    }
    pub fn refresh_session_mode(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let version = self.mode_requests.current(sid.clone());
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.session_mode(&sid).await.map(|m| (sid, m)) },
            move |this, res, cx| {
                if let Ok((sid, m)) = res
                    && this.mode_requests.is_current(&sid, version)
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
        self.mode_requests.bump(sid.clone());
        self.store.update(cx, |s, cx| {
            s.mode_sending.insert(sid.clone());
            cx.notify();
        });
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.set_session_mode(&id, mode).await },
            move |this, res, cx| {
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
    pub fn inherit_draft_permissions(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.draft_permission = None;
            cx.notify();
        });
    }
    /// Re-read the session's token and cost totals (one cheap request).
    pub fn refresh_usage(&mut self, sid: String, cx: &mut Context<Self>) {
        self.fetch_usage(sid, false, cx);
    }
    /// Everything the usage dialog shows, including account limits.
    pub fn refresh_usage_details(&mut self, sid: String, cx: &mut Context<Self>) {
        self.fetch_usage(sid, true, cx);
    }
    fn fetch_usage(&mut self, sid: String, details: bool, cx: &mut Context<Self>) {
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
            if details {
                self.usage_details_pending.insert(sid);
            }
            return;
        }
        let details = details || self.usage_details_pending.remove(&sid);
        // A loaded conversation already holds the per-model numbers.
        let history = details && self.store.read(cx).usage_needs_history(&sid);
        let revision = self.usage_revision.current(sid.clone());
        self.store.update(cx, |s, cx| {
            s.usage_loading.insert(sid.clone());
            s.usage_errors.remove(&sid);
            cx.emit(StoreEvent::UsageChanged {
                session_id: sid.clone(),
            });
        });
        let id = sid.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.usage_report(&id, &folder, details, history).await },
            move |this, res, cx| {
                if this.store.read(cx).session(&sid).is_none() {
                    // Deleted while in flight: don't resurrect its usage entry.
                    this.store.update(cx, |s, _| {
                        s.usage_loading.remove(&sid);
                    });
                    return;
                }
                let changed = !this.usage_revision.is_current(&sid, revision);
                this.store.update(cx, |s, cx| {
                    s.usage_loading.remove(&sid);
                    match res {
                        Ok(mut report) => {
                            let old = s.usage.remove(&sid);
                            if let Some(old) = &old
                                && changed
                            {
                                report.tokens = old.tokens.clone();
                                report.cost = old.cost;
                            }
                            // A summary keeps what the last details fetch found.
                            if !details && let Some(old) = old {
                                report = crate::api::features::UsageReport {
                                    tokens: report.tokens,
                                    cost: report.cost,
                                    ..old
                                };
                            }
                            s.usage.insert(sid.clone(), report);
                        }
                        Err(e) => {
                            s.usage_errors.insert(sid.clone(), format!("{e:#}"));
                        }
                    }
                    cx.emit(StoreEvent::UsageChanged {
                        session_id: sid.clone(),
                    });
                });
                // A newer live total (`changed`) already replaced the REST one
                // above, so only a dialog request still needs another read.
                if this.usage_details_pending.contains(&sid) {
                    this.fetch_usage(sid, false, cx);
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
        // Same shape as before the scope picker went away, so marks survive.
        format!("{dir}\nWorking tree:\n{file}")
    }
    /// Drop "viewed" marks of files in `dir` that have no changes any more,
    /// so the map doesn't grow with every file ever reviewed.
    pub(super) fn prune_viewed(
        &mut self,
        dir: &std::path::Path,
        files: &[crate::api::model::FileDiff],
        cx: &mut Context<Self>,
    ) {
        let prefix = format!("{}\nWorking tree:\n", dir.display());
        let stale = |key: &String| {
            key.strip_prefix(&prefix)
                .is_some_and(|file| !files.iter().any(|f| f.file == file))
        };
        if !self.settings.viewed.keys().any(stale) {
            return;
        }
        self.update_settings(cx, |s| s.viewed.retain(|k, _| !stale(k)));
        self.store
            .update(cx, |_, cx| cx.emit(StoreEvent::ViewedChanged));
    }
    pub fn mark_viewed(&mut self, sid: String, file: String, hash: u64, cx: &mut Context<Self>) {
        let key = self.viewed_key(&sid, &file, cx);
        self.update_settings(cx, |s| {
            if s.viewed.get(&key) == Some(&hash) {
                s.viewed.remove(&key);
            } else {
                s.viewed.insert(key, hash);
            }
        });
        self.store
            .update(cx, |_, cx| cx.emit(StoreEvent::ViewedChanged));
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
    /// Open an agent file (found by `agents::discover`, possibly outside the
    /// project) in the configured editor.
    pub fn open_agent_file(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let editor = self
            .settings
            .editor
            .clone()
            .unwrap_or_else(|| "code".into());
        self.run(
            cx,
            async move {
                let mut child = tokio::process::Command::new(editor)
                    .arg("--")
                    .arg(path)
                    .spawn()?;
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

    /// Agent files changed on disk: re-read the folder's agents now instead
    /// of waiting for opencode's `agent.updated`.
    pub fn agents_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(folder) = self.store.read(cx).catalog_folder() {
            self.refresh_catalog(folder, cx);
        }
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
            // The store forgets its own state when it applies the event.
            self.mode_requests.forget(session_id);
            self.usage_revision.forget(session_id);
            self.model_choices.forget(session_id);
            self.agent_choices.forget(session_id);
            self.usage_details_pending.remove(session_id);
            self.fetched_epoch.remove(session_id);
            self.update_settings(cx, |s| {
                s.unread.remove(session_id);
            });
            cx.dismiss_system_notification(&format!("ocdesk:{session_id}"));
            return;
        }
        // Server reports keep optimistic rollbacks pointed at its value.
        match &env.event {
            Event::ModelSelected { session_id, model } => {
                self.model_choices.observed(session_id, model)
            }
            Event::AgentSelected { session_id, agent } => {
                self.agent_choices.observed(session_id, agent)
            }
            _ => {}
        }
        if let Event::UsageUpdated {
            session_id,
            tokens,
            cost,
        } = &env.event
        {
            self.usage_revision.bump(session_id.clone());
            self.store.update(cx, |s, cx| {
                let r = s.usage.entry(session_id.clone()).or_default();
                r.tokens = crate::api::features::Tokens::parse(tokens);
                r.cost = *cost;
                // Per step: an event (header meter, usage dialog), not a
                // store-wide redraw.
                cx.emit(StoreEvent::UsageChanged {
                    session_id: session_id.clone(),
                });
            });
        }
        let Some(attention) = crate::notifications::attention(&env.event, self.store.read(cx))
        else {
            return;
        };
        let key = format!("{}:{:?}", attention.key, env.seq);
        // Dedupes replays after a reconnect; recent keys are all that matter.
        if self.attention_seen.len() > 4096 {
            self.attention_seen.clear();
        }
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
        let folders = self.store.read(cx).folders.clone();
        self.grant_requests += 1;
        let request = self.grant_requests;
        self.store.update(cx, |s, cx| {
            s.saved_grants_loading = true;
            s.saved_grants_error = None;
            cx.notify();
        });
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.saved_grants(&folders).await },
            move |this, res, cx| {
                if this.grant_requests != request {
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
        self.grant_requests += 1;
        self.store.update(cx, |s, cx| {
            s.saved_grants_loading = false;
            s.grant_sending.insert(id.clone());
            cx.notify();
        });
        let target = id.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.revoke_grant(&target).await },
            move |this, res, cx| {
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

// ── navigation, palette and sidebar intents ──────────────────────────────

impl Controller {
    /// Ctrl+O: normal → verbose → quiet.
    /// Returns what to tell the user.
    pub fn cycle_density(&mut self, cx: &mut Context<Self>) -> String {
        let next = self.settings.density.next();
        self.update_settings(cx, |s| s.density = next);
        format!("Transcript: {}", next.label())
    }

    pub fn set_density(&mut self, density: crate::settings::Density, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.density = density);
    }

    /// The permission mode the composer shows for the selected session or
    /// the draft.
    pub fn current_permission_mode(&self, cx: &App) -> Option<PermissionMode> {
        let store = self.store.read(cx);
        match &store.selected {
            Some(sid) => store.session_modes.get(sid).copied().flatten(),
            None => store.draft_permission,
        }
    }

    pub fn cycle_permission_mode(&mut self, cx: &mut Context<Self>) -> String {
        let current = self.current_permission_mode(cx);
        let all = PermissionMode::ALL;
        let next = current
            .and_then(|m| all.iter().position(|x| *x == m))
            .map_or(all[0], |i| all[(i + 1) % all.len()]);
        self.set_permission_mode(next, cx);
        format!("Permissions: {}", next.label())
    }

    /// Next reasoning variant of the current model (wrapping to the
    /// model's default).
    pub fn cycle_effort(&mut self, cx: &mut Context<Self>) -> String {
        let store = self.store.read(cx);
        let Some(model) = store.effective_model() else {
            return "Choose a model first".into();
        };
        let variants = store
            .target_catalog()
            .and_then(|c| c.model(&model))
            .map(|m| m.variants.clone())
            .unwrap_or_default();
        if variants.is_empty() {
            return "This model has no reasoning levels".into();
        }
        let next = match model
            .variant
            .as_ref()
            .and_then(|v| variants.iter().position(|x| x == v))
        {
            None => Some(variants[0].clone()),
            Some(i) if i + 1 < variants.len() => Some(variants[i + 1].clone()),
            Some(_) => None,
        };
        let label = next.clone().unwrap_or_else(|| "default".into());
        self.choose_model(
            ModelRef {
                variant: next,
                ..model
            },
            cx,
        );
        format!("Reasoning effort: {label}")
    }

    /// The selected session's last reply as plain text.
    pub fn last_reply(&self, cx: &App) -> Option<String> {
        use crate::api::model::{Block, BlockKind, Message};
        let conv = self.store.read(cx).selected_conversation()?;
        let last_user = conv
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::User(_)))
            .map_or(0, |i| i + 1);
        let text: Vec<&str> = conv.messages[last_user..]
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            })
            .flat_map(|a| a.blocks.iter())
            .filter_map(|b| match b {
                Block::Prose {
                    kind: BlockKind::Text,
                    text,
                    ..
                } if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            })
            .collect();
        (!text.is_empty()).then(|| text.join("\n\n"))
    }

    pub fn toggle_pin(&mut self, sid: String, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| {
            if let Some(i) = s.pinned.iter().position(|p| *p == sid) {
                s.pinned.remove(i);
            } else {
                s.pinned.insert(0, sid);
            }
        });
    }

    /// Archive (`true`) or restore a session. The server removes an
    /// archived session from the list; restoring re-reads the list.
    pub fn set_archived(&mut self, sid: String, archived: bool, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        if archived && self.store.read(cx).selected.as_deref() == Some(sid.as_str()) {
            self.select_session(None, cx);
        }
        let id = sid.clone();
        self.run(
            cx,
            async move { api.set_archived(&id, archived).await },
            move |this, res, cx| match res {
                Ok(()) if !archived => this.refresh_sessions(cx),
                Ok(()) => {}
                Err(e) => this.notice(format!("Could not update {sid}: {e:#}"), cx),
            },
        );
    }
}

impl Controller {
    /// The API client and the Tokio runtime, for views that run their own
    /// long-lived I/O (the terminal). `None` while opencode isn't running.
    pub fn api_and_runtime(&self) -> Option<(crate::api::Api, tokio::runtime::Handle)> {
        Some((self.api.clone()?, self.rt.clone()))
    }
}
