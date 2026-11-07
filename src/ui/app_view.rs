//! M0 root view: sidecar status plus a test-prompt button. Replaced by the
//! sidebar | conversation layout in M1.

use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::{Api, Event};
use crate::sidecar::{self, Sidecar, SidecarConfig};

enum Status {
    Starting,
    Ready {
        sidecar: Sidecar,
        api: Api,
    },
    Failed(String),
    Exited {
        code: Option<i32>,
        last_lines: Vec<String>,
    },
}

pub struct AppView {
    rt: tokio::runtime::Handle,
    status: Status,
    connected: bool,
    events_seen: usize,
    last_event: SharedString,
    session_id: Option<String>,
    first_frame_logged: bool,
    _tasks: Vec<Task<()>>,
}

impl AppView {
    pub fn new(rt: tokio::runtime::Handle, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            rt,
            status: Status::Starting,
            connected: false,
            events_seen: 0,
            last_event: "".into(),
            session_id: None,
            first_frame_logged: false,
            _tasks: Vec::new(),
        };
        this.start_sidecar(cx);
        this
    }

    fn start_sidecar(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let res = async {
                let binary = sidecar::find_binary().ok_or_else(|| {
                    anyhow::anyhow!("opencode not found on PATH. Install: curl -fsSL https://opencode.ai/install | bash")
                })?;
                let working_dir = std::env::var_os("OCDESK_DIR")
                    .map(Into::into)
                    .unwrap_or(std::env::current_dir()?);
                sidecar::spawn(SidecarConfig { binary, working_dir }).await
            }
            .await;
            let _ = tx.send(res);
        });

        let task = cx.spawn(async move |this, cx| {
            let Ok(res) = rx.await else { return };
            let (sidecar, exit_rx) = match res {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("sidecar failed to start: {e:#}");
                    let _ = this.update(cx, |this, cx| {
                        this.status = Status::Failed(format!("{e:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let api = match Api::new(&sidecar) {
                Ok(api) => api,
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = Status::Failed(format!("{e:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let Ok(()) = this.update(cx, |this, cx| {
                this.status = Status::Ready {
                    sidecar,
                    api: api.clone(),
                };
                this.watch_exit(exit_rx, cx);
                this.pump_events(api, cx);
                cx.notify();
            }) else {
                return;
            };
        });
        self._tasks.push(task);
    }

    fn watch_exit(
        &mut self,
        exit_rx: tokio::sync::oneshot::Receiver<sidecar::SidecarExit>,
        cx: &mut Context<Self>,
    ) {
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Ok(exit) = exit_rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.status = Status::Exited {
                        code: exit.code,
                        last_lines: exit.last_lines,
                    };
                    this.connected = false;
                    cx.notify();
                });
            }
        }));
    }

    fn pump_events(&mut self, api: Api, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::unbounded();
        self.rt.spawn(async move {
            if let Err(e) = api.run_events(tx).await {
                tracing::error!("event pump ended: {e:#}");
            }
        });
        let probe = std::env::var("OCDESK_PROBE_PROMPT").ok();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let mut probe = probe;
            while let Some(ev) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.events_seen += 1;
                    this.last_event = summarize(&ev).into();
                    if matches!(ev, Event::Connected) {
                        this.connected = true;
                        #[cfg(debug_assertions)]
                        if std::env::var_os("OCDESK_DEBUG_PANIC").is_some() {
                            panic!("OCDESK_DEBUG_PANIC: deliberate panic to test sidecar teardown");
                        }
                        if let Some(p) = probe.take() {
                            this.send_test_prompt(p, cx);
                        }
                    }
                    if let Event::SessionIdle { session_id } = &ev {
                        tracing::info!(%session_id, "session idle");
                        if std::env::var_os("OCDESK_EXIT_ON_IDLE").is_some()
                            && this.session_id.as_deref() == Some(session_id)
                        {
                            this.log_messages_then_quit(cx);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn send_test_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let Status::Ready { api, .. } = &self.status else {
            return;
        };
        let api = api.clone();
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let res = async {
                let s = api.create_session(Some("ocdesk M0 probe".into())).await?;
                tracing::info!(session_id = %s.id, "created session");
                api.prompt_text(&s.id, &text).await?;
                tracing::info!(session_id = %s.id, "prompt submitted");
                anyhow::Ok(s.id)
            }
            .await;
            let _ = tx.send(res);
        });
        self._tasks
            .push(cx.spawn(async move |this, cx| match rx.await {
                Ok(Ok(id)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.session_id = Some(id);
                        cx.notify();
                    });
                }
                Ok(Err(e)) => tracing::error!("test prompt failed: {e:#}"),
                Err(_) => {}
            }));
    }

    fn abort(&mut self, cx: &mut Context<Self>) {
        let (Status::Ready { api, .. }, Some(sid)) = (&self.status, self.session_id.clone()) else {
            return;
        };
        let api = api.clone();
        self.rt.spawn(async move {
            match api.abort(&sid).await {
                Ok(aborted) => tracing::info!(%sid, aborted, "abort requested"),
                Err(e) => tracing::error!("abort failed: {e:#}"),
            }
        });
        cx.notify();
    }

    /// Verification helper: log the authoritative REST view, then quit.
    fn log_messages_then_quit(&mut self, cx: &mut Context<Self>) {
        let (Status::Ready { api, .. }, Some(sid)) = (&self.status, self.session_id.clone()) else {
            return;
        };
        let api = api.clone();
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            match api.messages(&sid).await {
                Ok(msgs) => tracing::info!(
                    "REST messages after idle: {}",
                    serde_json::to_string(&msgs).unwrap_or_default()
                ),
                Err(e) => tracing::error!("list messages failed: {e:#}"),
            }
            let _ = tx.send(());
        });
        self._tasks.push(cx.spawn(async move |_, cx| {
            let _ = rx.await;
            cx.update(|cx| cx.quit());
        }));
    }
}

fn summarize(ev: &Event) -> String {
    match ev {
        Event::Other(t) => t.clone(),
        Event::PartDelta { field, delta, .. } => {
            format!("part delta ({field}, {} bytes)", delta.len())
        }
        other => {
            let s = format!("{other:?}");
            s.chars().take(120).collect()
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            if let Some(t) = crate::STARTED.get() {
                tracing::info!(ms = t.elapsed().as_millis() as u64, "first frame rendered");
            }
        }
        let theme = cx.theme();
        let (headline, detail): (SharedString, Vec<SharedString>) = match &self.status {
            Status::Starting => ("Starting opencode…".into(), vec![]),
            Status::Ready { sidecar, .. } => (
                format!("opencode {} at {}", sidecar.version, sidecar.base_url).into(),
                vec![
                    format!("pid {}", sidecar.pid).into(),
                    format!(
                        "events: {} · {}",
                        self.events_seen,
                        if self.connected {
                            "connected"
                        } else {
                            "connecting"
                        }
                    )
                    .into(),
                    format!("last: {}", self.last_event).into(),
                    format!("session: {}", self.session_id.as_deref().unwrap_or("-")).into(),
                ],
            ),
            Status::Failed(e) => ("opencode failed to start".into(), vec![e.clone().into()]),
            Status::Exited { code, last_lines } => (
                format!("opencode exited ({code:?})").into(),
                last_lines.iter().map(|l| l.clone().into()).collect(),
            ),
        };
        let ready = matches!(self.status, Status::Ready { .. }) && self.connected;

        v_flex()
            .size_full()
            .p_6()
            .gap_3()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(div().text_xl().child("ocdesk"))
            .child(div().child(headline))
            .children(
                detail
                    .into_iter()
                    .map(|d| div().text_sm().text_color(theme.muted_foreground).child(d)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("test-prompt")
                            .primary()
                            .label("Send test prompt")
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send_test_prompt(
                                    "Say hello in one short sentence.".into(),
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("abort")
                            .label("Abort")
                            .disabled(!ready || self.session_id.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.abort(cx))),
                    ),
            )
    }
}
