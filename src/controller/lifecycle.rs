//! Sidecar lifecycle (start, crash detection, auto-restart), the SSE event
//! pump, and resynchronisation after every (re)connect.

use super::*;

impl Controller {
    /// Kill whatever is running and start a fresh sidecar.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.restarts.reset();
        sidecar::kill_sidecar();
        self.start_sidecar(cx);
    }

    pub(super) fn start_sidecar(&mut self, cx: &mut Context<Self>) {
        self.restart_timer = None;
        self.requests.restart();
        self.fetched_epoch.clear();
        // Their completions are discarded with the old generation.
        self.model_choices.clear();
        self.agent_choices.clear();
        let generation = self.requests.generation();
        self.api = None;
        self.store.update(cx, |s, cx| {
            s.cancel_history_loads();
            s.submitting.clear();
            s.interrupting.clear();
            s.abandon_replies(cx);
            for dir in s.diff_status.abandon_all() {
                cx.emit(StoreEvent::DiffsChanged { dir });
            }
            s.mode_sending.clear();
            s.usage_status.abandon_all();
            s.grant_sending.clear();
            s.saved_grants_status.abandon();
            cx.notify();
        });
        self.stop_pump();
        self.exit_watch = None;
        self.store
            .update(cx, |s, cx| s.set_connection(Connection::Starting, cx));

        // Every session carries its own location, so the sidecar's own
        // directory doesn't matter.
        let working_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let binary = sidecar::find_binary().ok_or_else(|| {
                    anyhow!(
                        "opencode 2.x is not installed or not on PATH. \
                         Install it with: {}",
                        sidecar::INSTALL_HINT
                    )
                })?;
                sidecar::spawn(SidecarConfig {
                    binary,
                    working_dir,
                })
                .await
            },
            move |this, res, cx| {
                let started = res.and_then(|(sc, exit_rx)| Ok((Api::new(&sc)?, sc, exit_rx)));
                let (api, sc, exit_rx) = match started {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(op = "start_sidecar", ?generation, error = %format!("{e:#}"), "sidecar failed to start");
                        this.store.update(cx, |s, cx| {
                            s.set_connection(
                                Connection::SidecarDown {
                                    message: format!("{e:#}"),
                                    last_lines: vec![],
                                },
                                cx,
                            )
                        });
                        return;
                    }
                };
                this.store.update(cx, |s, cx| {
                    s.opencode_version = Some(sc.version.clone());
                    s.set_connection(Connection::Connecting, cx);
                });
                this.api = Some(api.clone());
                this.watch_exit(generation, exit_rx, cx);
                this.start_pump(api, cx);
            },
        );
    }

    fn watch_exit(
        &mut self,
        generation: ServerGeneration,
        exit_rx: tokio::sync::oneshot::Receiver<sidecar::SidecarExit>,
        cx: &mut Context<Self>,
    ) {
        self.exit_watch = Some(cx.spawn(async move |this, cx| {
            let Ok(exit) = exit_rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.requests.generation() != generation {
                    return;
                }
                this.api = None;
                this.stop_pump();
                let response = this.restarts.crashed(Instant::now());
                let message = format!("opencode stopped unexpectedly (exit code {:?})", exit.code);
                this.store.update(cx, |s, cx| {
                    s.set_connection(
                        Connection::SidecarDown {
                            message,
                            last_lines: exit.last_lines,
                        },
                        cx,
                    );
                    // Nothing is running any more.
                    s.set_active(vec![], cx);
                });
                match response {
                    CrashResponse::Restart { crashes } => {
                        tracing::warn!(op = "start_sidecar", ?generation, crashes, code = ?exit.code, "restarting sidecar automatically");
                        this.restart_timer = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(RESTART_DELAY).await;
                            let _ = this.update(cx, |this, cx| {
                                // This task is the timer: let it finish instead
                                // of dropping (cancelling) itself mid-poll.
                                if let Some(me) = this.restart_timer.take() {
                                    me.detach();
                                }
                                this.start_sidecar(cx);
                            });
                        }));
                    }
                    CrashResponse::GiveUp { crashes } => {
                        tracing::error!(op = "start_sidecar", ?generation, crashes, code = ?exit.code, "sidecar keeps crashing; not restarting");
                    }
                }
            });
        }));
    }

    fn start_pump(&mut self, api: Api, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::channel(32);
        let handle = self
            .rt
            .spawn(async move { api.run_events(tx).await })
            .abort_handle();
        let task = cx.spawn(async move |this, cx| {
            while let Some(env) = rx.next().await {
                // Fold everything already queued in one update, so a burst
                // of streamed deltas flushes effects (and redraws) once.
                let mut batch = vec![env];
                while batch.len() < 256
                    && let Ok(env) = rx.try_recv()
                {
                    batch.push(env);
                }
                let applied = this.update(cx, |this, cx| {
                    for env in batch {
                        this.on_event(env, cx);
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        });
        self.stop_pump();
        self.pump = Some((handle, task));
    }

    /// Dropping a tokio `AbortHandle` does not stop the task; abort explicitly.
    pub(super) fn stop_pump(&mut self) {
        if let Some((handle, _task)) = self.pump.take() {
            handle.abort();
        }
    }

    pub(super) fn on_event(&mut self, env: Envelope, cx: &mut Context<Self>) {
        self.product_event(&env, cx);
        match env.event {
            Event::Connected => {
                self.requests.connection_changed();
                #[cfg(debug_assertions)]
                if std::env::var_os("OCDESK_DEBUG_PANIC").is_some() {
                    panic!("OCDESK_DEBUG_PANIC: deliberate panic to test sidecar teardown");
                }
                self.store
                    .update(cx, |s, cx| s.set_connection(Connection::Connected, cx));
                self.resync(cx);
                // Watches may have fired while we were away.
                self.files_changed(None, None, false, cx);
            }
            Event::FileChanged { file, removed } => {
                self.files_changed(env.directory, Some(file), removed, cx)
            }
            Event::Disconnected => {
                self.requests.connection_changed();
                self.store.update(cx, |s, cx| {
                    s.cancel_history_loads();
                    s.abandon_replies(cx);
                    cx.notify();
                });
                self.store.update(cx, |s, cx| {
                    if s.connection() == Connection::Connected {
                        s.set_connection(Connection::Reconnecting, cx);
                    }
                });
            }
            Event::CatalogChanged | Event::ToolingChanged | Event::WorktreesChanged { .. } => {
                self.schedule_catalog_refresh(cx)
            }
            _ => {
                let turn_over = match &env.event {
                    Event::ExecutionEnded { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                };
                // A tool may have edited files: revalidate open tabs (cheap
                // 304s), whether or not a watch reports it. Known read-only
                // tools can't have; unknown ones (MCP) might.
                let touched = match &env.event {
                    Event::ToolSucceeded {
                        session_id,
                        message_id,
                        call_id,
                        ..
                    } => {
                        let name = self
                            .store
                            .read(cx)
                            .tool_name(session_id, message_id, call_id);
                        (!name.is_some_and(is_read_only_tool)).then(|| env.directory.clone())
                    }
                    _ => None,
                };
                self.store.update(cx, |s, cx| s.apply(env, cx));
                if let Some(dir) = touched {
                    self.files_changed(dir, None, false, cx);
                }
                if let Some(session_id) = turn_over {
                    self.on_turn_over(session_id, cx);
                }
            }
        }
    }

    /// A turn finished (`session.execution.succeeded|failed|interrupted`).
    fn on_turn_over(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.refresh_usage(session_id.clone(), cx);
        let reload = self
            .store
            .update(cx, |s, _| s.needs_reload.remove(&session_id));
        if reload
            && self
                .store
                .read(cx)
                .conversations
                .get(&session_id)
                .is_some_and(|c| c.loaded())
        {
            self.load_messages(session_id.clone(), cx);
        }
        // Events keep approvals current; re-read only what is still shown,
        // in case the turn ended without a replied/cancelled event.
        if self.store.read(cx).permissions.is_tracked(&session_id) {
            self.refresh_permissions(session_id.clone(), cx);
        }
        if self.store.read(cx).forms.is_tracked(&session_id) {
            self.refresh_forms(session_id.clone(), cx);
        }
        self.diff_maybe_changed(&session_id, cx);
        #[cfg(debug_assertions)]
        if std::env::var_os("OCDESK_VERIFY_ON_IDLE").is_some() {
            self.verify_against_rest(session_id, cx);
        }
        // Verification hook for dev/teardown-test.sh: quit through the normal
        // on_app_quit path once a turn finishes.
        if std::env::var_os("OCDESK_EXIT_ON_IDLE").is_some() {
            cx.quit();
        }
    }

    /// After every (re)connect frames may have been missed: re-fetch the
    /// session list, busy states, and the selected conversation.
    pub(super) fn refresh_active(&mut self, cx: &mut Context<Self>) {
        self.read_active(0, cx);
    }

    fn read_active(&mut self, attempt: u32, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let revision = self.store.read(cx).snapshot_revision;
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move { api.active().await },
            move |this, res, cx| {
                if this.store.read(cx).snapshot_revision != revision
                    && this.retry_snapshot(attempt, cx, move |this, cx| {
                        this.read_active(attempt + 1, cx)
                    })
                {
                    return;
                }
                match res {
                    Ok(active) => this.store.update(cx, |s, cx| s.set_active(active, cx)),
                    Err(e) => tracing::warn!(op = "active", error = %format!("{e:#}"), "could not read active sessions"),
                }
            },
        );
    }

    fn resync(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        self.store
            .update(cx, |s, _| s.invalidate_except(selected.as_deref()));
        self.fetched_epoch.clear();
        if let Some(id) = &selected {
            self.fetched_epoch.insert(id.clone(), self.requests.epoch());
        }
        self.refresh_sessions(cx);
        self.refresh_active(cx);
        self.refresh_saved_grants(cx);
        // Approvals of every listed session (children included) are read per
        // folder by `refresh_sessions`.
        if let Some(id) = selected {
            self.refresh_session_mode(id.clone(), cx);
            self.refresh_usage(id.clone(), cx);
            self.refresh_inbox(id.clone(), cx);
            self.refresh_tasks(id.clone(), cx);
            self.load_messages(id.clone(), cx);
            if let Some(dir) = self.store.read(cx).selected_diff_dir()
                && self.store.read(cx).diffs.contains_key(&dir)
            {
                self.refresh_folder_diff(dir, cx);
            }
        }
        // Prime every folder's location: a fresh sidecar registers a project
        // only when the directory is first mentioned, and until then reads for
        // that location's sessions 404 (LocationNotFoundError).
        let folders = self.store.read(cx).folders.clone();
        if let Some(api) = self.api.clone() {
            self.run(
                cx,
                async move {
                    for f in &folders {
                        let _ = api.location(f).await;
                    }
                    Ok::<(), anyhow::Error>(())
                },
                |this, _res, cx| this.refresh_catalogs(cx),
            );
        }
    }

    /// Debug check (OCDESK_VERIFY_ON_IDLE): the streamed state of a loaded
    /// conversation must equal the server's REST projection after a turn.
    #[cfg(debug_assertions)]
    fn verify_against_rest(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let sid = session_id.clone();
        // Give the projection a moment after the terminal event.
        let fut = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            api.messages(&sid).await
        };
        self.run(cx, fut, move |this, res, cx| {
            let Ok(history) = res else { return };
            let store = this.store.read(cx);
            let Some(conv) = store.conversations.get(&session_id).filter(|c| c.loaded()) else { return };
            if conv.messages == history {
                tracing::info!(%session_id, messages = history.len(), "verify: store matches REST");
            } else {
                tracing::error!(%session_id, local = ?conv.messages, remote = ?history, "verify: store MISMATCH with REST");
            }
        });
    }
}
