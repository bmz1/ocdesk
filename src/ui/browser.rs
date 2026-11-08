//! Browser panel (right panel tab): the live view of the browser an agent
//! drives with agent-browser (see `crate::browser`).
//!
//! It follows the agent-browser session the selected conversation used
//! last (from its `agent-browser …` shell calls), else the first live one;
//! the picker pins another. The stream is open only while the tab shows.
//! Control mode forwards clicks, scrolling and typing to the page.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::Sender;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{Block, Message};
use crate::browser::{self, LiveSession, SessionKey, StreamMsg, Tab};
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};

const POLL: Duration = Duration::from_millis(1500);
const MAX_FPS: u32 = 12;
const INSTALL: &str = "npm i -g agent-browser && agent-browser install";

struct Live {
    key: SessionKey,
    port: u16,
    input: Option<Sender<String>>,
    _task: Task<()>,
}

pub struct BrowserPanel {
    store: Entity<Store>,
    controller: Entity<Controller>,
    /// Live agent-browser sessions (polled).
    sessions: Vec<LiveSession>,
    /// The user's pick; `None` follows the conversation.
    pinned: Option<SessionKey>,
    visible: bool,
    installed: bool,
    live: Option<Live>,
    frame: Option<Arc<RenderImage>>,
    /// Page viewport in CSS pixels, and the frame's pixel size.
    device: (f32, f32),
    frame_size: (u32, u32),
    /// Frames replaced since the last paint; freed from the GPU atlas.
    stale: Vec<Arc<RenderImage>>,
    url: Option<String>,
    tabs: Vec<Tab>,
    status: Option<String>,
    control: bool,
    focus: FocusHandle,
    /// Where the frame was painted, for mapping clicks to page pixels.
    view_bounds: Rc<Cell<Bounds<Pixels>>>,
    _poll: Task<()>,
    _subs: Vec<Subscription>,
}

impl BrowserPanel {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| {
            if matches!(ev, StoreEvent::SelectionChanged) {
                this.pinned = None;
                this.sync(cx);
            }
        })];
        // Cheap: one directory listing. Keeps the tab's activity dot and the
        // session list current even while the panel is closed.
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let sessions = cx
                    .background_executor()
                    .spawn(async { browser::live_sessions() })
                    .await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if sessions != this.sessions {
                        this.sessions = sessions;
                        this.sync(cx);
                        cx.notify();
                    } else if this.visible {
                        // The conversation may have moved to another session.
                        this.sync(cx);
                    }
                }) else {
                    return;
                };
                cx.background_executor().timer(POLL).await;
            }
        });
        Self {
            store,
            controller,
            sessions: browser::live_sessions(),
            pinned: None,
            visible: false,
            installed: browser::installed(),
            live: None,
            frame: None,
            device: (0., 0.),
            frame_size: (0, 0),
            stale: Vec::new(),
            url: None,
            tabs: Vec::new(),
            status: None,
            control: false,
            focus: cx.focus_handle(),
            view_bounds: Rc::new(Cell::new(Bounds::default())),
            _poll: poll,
            _subs: subs,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Whether any agent-browser session is running (the tab's dot).
    pub fn active(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// The tab was shown or hidden: open or close the stream.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible != self.visible {
            self.visible = visible;
            if visible {
                self.installed = browser::installed();
            }
            self.sync(cx);
        }
    }

    /// Show a specific session (from a transcript row).
    pub fn show(&mut self, key: Option<SessionKey>, cx: &mut Context<Self>) {
        self.pinned = key;
        self.sync(cx);
    }

    /// The session the selected conversation used last.
    fn conversation_session(&self, cx: &App) -> Option<SessionKey> {
        let s = self.store.read(cx);
        let conv = s.selected_conversation()?;
        conv.messages.iter().rev().find_map(|m| {
            let Message::Assistant(a) = m else {
                return None;
            };
            a.blocks.iter().rev().find_map(|b| match b {
                Block::Tool(t) => t
                    .input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .and_then(|c| browser::invocations(c).pop())
                    .map(|i| i.session),
                _ => None,
            })
        })
    }

    /// Which live session to show.
    fn target(&self, cx: &App) -> Option<LiveSession> {
        let find = |k: &SessionKey| self.sessions.iter().find(|s| s.key() == *k).cloned();
        self.pinned
            .as_ref()
            .and_then(find)
            .or_else(|| self.conversation_session(cx).as_ref().and_then(find))
            .or_else(|| self.sessions.first().cloned())
    }

    /// Connect to the target while visible; disconnect otherwise.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let want = if self.visible { self.target(cx) } else { None };
        let same = match (&self.live, &want) {
            (Some(l), Some(w)) => l.key == w.key() && l.port == w.port,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.disconnect();
        if let Some(target) = want {
            self.connect(target, cx);
        }
        cx.notify();
    }

    fn disconnect(&mut self) {
        self.live = None;
        self.control = false;
        if let Some(f) = self.frame.take() {
            self.stale.push(f);
        }
        self.url = None;
        self.tabs.clear();
        self.status = None;
    }

    fn connect(&mut self, target: LiveSession, cx: &mut Context<Self>) {
        let rt = self.controller.read(cx).runtime();
        let port = target.port;
        self.status = Some("Connecting…".into());
        let opened = rt.spawn(async move { browser::connect(port, MAX_FPS).await });
        let task = cx.spawn(async move |this, cx| {
            let opened = opened.await.map_err(anyhow::Error::from).and_then(|r| r);
            let mut rx = match opened {
                Ok((tx, rx)) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(l) = &mut this.live {
                            l.input = Some(tx);
                        }
                        this.status = None;
                        cx.notify();
                    });
                    rx
                }
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = Some(format!("Can't reach the browser stream: {e:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            while let Some(msg) = rx.next().await {
                if this.update(cx, |this, cx| this.handle(msg, cx)).is_err() {
                    return;
                }
            }
        });
        self.live = Some(Live {
            key: target.key(),
            port,
            input: None,
            _task: task,
        });
    }

    fn handle(&mut self, msg: StreamMsg, cx: &mut Context<Self>) {
        match msg {
            StreamMsg::Frame(f) => {
                if let Some(old) = self.frame.replace(f.image) {
                    self.stale.push(old);
                }
                self.device = (f.device_width, f.device_height);
                self.frame_size = (f.width, f.height);
            }
            StreamMsg::Url(u) => self.url = Some(u),
            StreamMsg::Tabs(t) => {
                if let Some(a) = t.iter().find(|t| t.active) {
                    self.url = Some(a.url.clone());
                }
                self.tabs = t;
            }
            StreamMsg::Status { connected } => {
                self.status = (!connected).then(|| "The browser isn't connected.".into());
            }
            StreamMsg::Closed(reason) => {
                // `agent-browser close` resets the socket; that's not an error
                // worth showing.
                tracing::debug!(?reason, "browser stream closed");
                self.disconnect();
                self.status = Some("The browser was closed.".into());
            }
        }
        cx.notify();
    }

    fn send(&mut self, msgs: impl IntoIterator<Item = String>) {
        if let Some(tx) = self.live.as_mut().and_then(|l| l.input.as_mut()) {
            for m in msgs {
                let _ = tx.try_send(m);
            }
        }
    }

    /// A window position → page CSS pixels, if it's over the frame.
    fn page_point(&self, p: Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.view_bounds.get();
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0. || h <= 0. || !b.contains(&p) {
            return None;
        }
        // The frame is the page viewport; scale it by the width only:
        // `deviceHeight` can be the window's, not the viewport's (1280×720
        // reported for a 1280×577 viewport on agent-browser 0.41.2).
        let (fw, fh) = (self.frame_size.0 as f32, self.frame_size.1 as f32);
        let css = if fw > 0. && self.device.0 > 0. {
            self.device.0 / fw
        } else {
            1.
        };
        let x = f32::from(p.x - b.origin.x) / w * fw * css;
        let y = f32::from(p.y - b.origin.y) / h * fh * css;
        Some((x.round(), y.round()))
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.control || self.live.is_none() {
            return;
        }
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.platform || k.modifiers.alt {
            return;
        }
        let named = match k.key.as_str() {
            "enter" => Some(("Enter", "Enter", 13)),
            "backspace" => Some(("Backspace", "Backspace", 8)),
            "tab" => Some(("Tab", "Tab", 9)),
            "escape" => Some(("Escape", "Escape", 27)),
            "delete" => Some(("Delete", "Delete", 46)),
            "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
            "up" => Some(("ArrowUp", "ArrowUp", 38)),
            "right" => Some(("ArrowRight", "ArrowRight", 39)),
            "down" => Some(("ArrowDown", "ArrowDown", 40)),
            "home" => Some(("Home", "Home", 36)),
            "end" => Some(("End", "End", 35)),
            "pageup" => Some(("PageUp", "PageUp", 33)),
            "pagedown" => Some(("PageDown", "PageDown", 34)),
            _ => None,
        };
        if let Some((key, code, kc)) = named {
            self.send(browser::key(key, code, kc));
        } else if let Some(ch) = k.key_char.as_deref().filter(|c| !c.is_empty()) {
            self.send([browser::text(ch)]);
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn add_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        let path = browser::skill_path(&folder);
        let res = (|| {
            if path.exists() {
                anyhow::bail!("{} already exists", path.display());
            }
            std::fs::create_dir_all(path.parent().unwrap_or(&folder))?;
            std::fs::write(&path, browser::SKILL)?;
            anyhow::Ok(())
        })();
        let msg = match res {
            Ok(()) => format!("Added the agent-browser skill to {}", folder_name(&folder)),
            Err(e) => format!("{e:#}"),
        };
        super::kit::toast(window, cx, msg, None);
        cx.notify();
    }

    fn skill_present(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .target_folder()
            .is_some_and(|f| browser::skill_path(&f).exists())
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let current = self.live.as_ref().map(|l| l.key.clone());
        let label = current
            .as_ref()
            .map(|k| k.label())
            .unwrap_or_else(|| "No browser".into());
        let weak = cx.entity().downgrade();
        let sessions = self.sessions.clone();
        let following = self.pinned.is_none();
        let picker = super::kit::quiet_select("browser-session", label).dropdown_menu(
            move |mut menu, _, _| {
                let w = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new("Follow this conversation")
                        .checked(following)
                        .on_click(move |_, _, cx| {
                            let _ = w.update(cx, |t, cx| t.show(None, cx));
                        }),
                );
                if !sessions.is_empty() {
                    menu = menu.separator();
                }
                for s in &sessions {
                    let (w, key) = (weak.clone(), s.key());
                    menu = menu.item(
                        PopupMenuItem::new(key.label())
                            .checked(!following && current.as_ref() == Some(&key))
                            .on_click(move |_, _, cx| {
                                let k = key.clone();
                                let _ = w.update(cx, |t, cx| t.show(Some(k), cx));
                            }),
                    );
                }
                menu
            },
        );
        h_flex()
            .h_11()
            .flex_none()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                super::icons::globe()
                    .small()
                    .text_color(theme.muted_foreground)
                    .ml_1(),
            )
            .child(picker)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .children(self.url.clone()),
            )
            .when(self.live.is_some(), |el| {
                el.child(
                    Button::new("browser-control")
                        .icon(super::icons::mouse_pointer())
                        .ghost()
                        .small()
                        .selected(self.control)
                        .tooltip(if self.control {
                            "Control on: clicks, scrolling and typing go to the page"
                        } else {
                            "Take control: send clicks, scrolling and typing to the page"
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.control = !this.control;
                            if this.control {
                                this.focus.focus(window, cx);
                            }
                            cx.notify();
                        })),
                )
            })
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let skill = self.skill_present(cx);
        let para = |t: &'static str| {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(t)
        };
        let mut col = v_flex().p_4().gap_3();
        if !self.installed {
            col = col
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("agent-browser isn't installed"))
                .child(para("agent-browser is a browser automation CLI for agents. Install it, then agents can open pages, click and fill forms from the shell, and you watch here."))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.secondary.opacity(0.6))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .whitespace_normal()
                                .child(INSTALL),
                        )
                        .child(Clipboard::new("browser-install-copy").value(INSTALL)),
                )
                .child(para("Restart ocdesk afterwards if it can't find the command."));
        } else {
            col = col
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("No browser running"))
                .child(para("When an agent runs agent-browser (for example \"open the app and check the login page\"), its browser shows here live."));
        }
        col = col.child(if skill {
            para("This folder has the agent-browser skill (.opencode/skill/agent-browser), so agents know how to use it.")
                .into_any_element()
        } else {
            v_flex()
                .gap_2()
                .child(para("Add a skill so agents in this folder know when and how to use agent-browser."))
                .child(
                    Button::new("browser-add-skill")
                        .label("Add agent-browser skill to this folder")
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.add_skill(window, cx))),
                )
                .into_any_element()
        });
        if let Some(s) = &self.status {
            col = col.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(s.clone()),
            );
        }
        col.into_any_element()
    }

    fn render_live(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(frame) = self.frame.clone() else {
            return v_flex()
                .p_4()
                .gap_2()
                .items_center()
                .child(Spinner::new().small().color(theme.muted_foreground))
                .child(
                    div().text_xs().text_color(theme.muted_foreground).child(
                        self.status
                            .clone()
                            .unwrap_or_else(|| "Waiting for the page…".into()),
                    ),
                )
                .into_any_element();
        };
        let (w, h) = self.frame_size;
        let ratio = if h > 0 { w as f32 / h as f32 } else { 16. / 9. };
        let bounds = self.view_bounds.clone();
        let control = self.control;
        let view = div()
            .id("browser-frame")
            .w_full()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(if control { theme.primary } else { theme.border })
            // The canvas both paints the frame and records where, so
            // clicks map onto exactly the pixels shown.
            .child(
                canvas(
                    move |b, _, _| {
                        bounds.set(b);
                        b
                    },
                    move |_, b, window, _| {
                        let _ = window.paint_image(b, b, Corners::default(), frame, 0, false);
                    },
                )
                .w_full()
                .aspect_ratio(ratio),
            )
            .when(control, |el| {
                el.cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.focus.focus(window, cx);
                            let pt = this.page_point(ev.position);
                            if let Some((x, y)) = pt {
                                this.send(browser::click(x, y));
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll(cx.listener(|this, ev: &ScrollEvent, window, cx| {
                        let Some((x, y)) = this.page_point(ev.position) else {
                            return;
                        };
                        let d = ev.delta.pixel_delta(window.line_height());
                        // gpui: positive y scrolls up; CDP: positive deltaY scrolls down.
                        this.send([browser::wheel(x, y, -f32::from(d.x), -f32::from(d.y))]);
                        cx.stop_propagation();
                    }))
            });
        let tabs = self.tabs.len();
        v_flex()
            .p_2()
            .gap_2()
            .child(view)
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child({
                        let (fw, fh) = (self.frame_size.0 as f32, self.frame_size.1 as f32);
                        let css = if fw > 0. { self.device.0 / fw } else { 1. };
                        format!(
                            "{}×{}",
                            (fw * css).round() as u32,
                            (fh * css).round() as u32
                        )
                    })
                    .when(tabs > 1, |el| el.child(format!("{tabs} tabs")))
                    .child(div().flex_1())
                    .child(if control {
                        "Control on: the page gets your clicks and keys"
                    } else {
                        "Watching"
                    }),
            )
            .when_some(self.status.clone(), |el, s| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child(s))
            })
            .into_any_element()
    }
}

fn folder_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Free replaced frames from the GPU atlas.
        for old in self.stale.drain(..) {
            let _ = window.drop_image(old);
        }
        let body = if self.live.is_some() || self.frame.is_some() {
            self.render_live(cx)
        } else {
            self.render_empty(cx)
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .child(self.header(cx))
            .child(
                div()
                    .id("browser-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}
