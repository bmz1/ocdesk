//! Browser panel (right panel tab): the browser ocdesk runs for the
//! selected session's agent (`crate::browser`), live.
//!
//! Shows the session's focused tab through CDP's screencast (JPEG frames,
//! decoded off the UI thread, replaced frames freed from the GPU atlas).
//! The stream runs only while the tab shows. Take control sends clicks,
//! scrolling and typing to the page.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use base64::Engine as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::json;

use crate::browser::{Host, View};
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};

/// The screencast's maximum frame width (device pixels).
const STREAM_WIDTH: u32 = 1280;

struct Decoded {
    image: Arc<RenderImage>,
    size: (u32, u32),
    device: (f32, f32),
}

pub struct BrowserPanel {
    store: Entity<Store>,
    host: Host,
    visible: bool,
    view: View,
    /// (session, tab) the stream shows.
    streaming: Option<(String, String)>,
    stream_task: Option<Task<()>>,
    frame: Option<Decoded>,
    stale: Vec<Arc<RenderImage>>,
    control: bool,
    focus: FocusHandle,
    view_bounds: Rc<Cell<Bounds<Pixels>>>,
    _watch: Task<()>,
    _subs: Vec<Subscription>,
}

impl BrowserPanel {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = controller.read(cx).browser.clone();
        let mut changed = host.subscribe();
        let watch = cx.spawn(async move |this, cx| {
            while changed.changed().await.is_ok() {
                if this.update(cx, |this, cx| this.sync(cx)).is_err() {
                    return;
                }
            }
        });
        let subs = vec![cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| {
            if matches!(ev, StoreEvent::SelectionChanged) {
                this.control = false;
                this.sync(cx);
            }
        })];
        Self {
            store,
            host,
            visible: false,
            view: View::default(),
            streaming: None,
            stream_task: None,
            frame: None,
            stale: Vec::new(),
            control: false,
            focus: cx.focus_handle(),
            view_bounds: Rc::new(Cell::new(Bounds::default())),
            _watch: watch,
            _subs: subs,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Any tab open for the selected session (the tab button's dot).
    pub fn active(&self) -> bool {
        !self.view.tabs.is_empty()
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible != self.visible {
            self.visible = visible;
            self.sync(cx);
        }
    }

    fn session(&self, cx: &App) -> Option<String> {
        self.store.read(cx).selected.clone()
    }

    /// Refresh the view and (re)start or stop the stream.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let session = self.session(cx);
        self.view = session
            .as_deref()
            .map(|s| self.host.view(s))
            .unwrap_or_default();
        let want = match (&session, &self.view.focused) {
            (Some(s), Some(t)) if self.visible => Some((s.clone(), t.clone())),
            _ => None,
        };
        if want != self.streaming || (want.is_some() && self.stream_task.is_none()) {
            self.stream_task = None;
            if let Some(f) = self.frame.take() {
                self.stale.push(f.image);
            }
            self.streaming = want.clone();
            if let Some((s, t)) = want {
                self.start_stream(&s, &t, cx);
            }
        }
        cx.notify();
    }

    fn start_stream(&mut self, session: &str, tab: &str, cx: &mut Context<Self>) {
        let Some(mut rx) = self.host.screencast(session, tab, STREAM_WIDTH) else {
            return;
        };
        self.stream_task = Some(cx.spawn(async move |this, cx| {
            while let Some(frame) = rx.recv().await {
                let decoded = cx
                    .background_executor()
                    .spawn(async move {
                        decode(&frame.jpeg_base64).map(|(img, size)| (img, size, frame))
                    })
                    .await;
                let Some((image, size, frame)) = decoded else {
                    continue;
                };
                let ok = this.update(cx, |this, cx| {
                    let d = Decoded {
                        image,
                        size,
                        device: (frame.device_width, frame.device_height),
                    };
                    if let Some(old) = this.frame.replace(d) {
                        this.stale.push(old.image);
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    return;
                }
            }
        }));
    }

    /// Window position → page CSS pixels, if over the frame. Scaled by the
    /// width only: frames are the viewport, `deviceWidth` is in CSS pixels.
    fn page_point(&self, p: Point<Pixels>) -> Option<(f64, f64)> {
        let f = self.frame.as_ref()?;
        let b = self.view_bounds.get();
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0. || h <= 0. || !b.contains(&p) {
            return None;
        }
        let css = if f.size.0 > 0 {
            f.device.0 / f.size.0 as f32
        } else {
            1.
        };
        let x = f32::from(p.x - b.origin.x) / w * f.size.0 as f32 * css;
        let y = f32::from(p.y - b.origin.y) / h * f.size.1 as f32 * css;
        Some((x.round() as f64, y.round() as f64))
    }

    fn input(&self, method: &'static str, params: serde_json::Value) {
        if let Some((s, t)) = &self.streaming {
            self.host.input(s, t, method, params);
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.control || self.streaming.is_none() {
            return;
        }
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.platform || k.modifiers.alt {
            return;
        }
        let named = match k.key.as_str() {
            "enter" => Some(("Enter", 13, Some("\r"))),
            "backspace" => Some(("Backspace", 8, None)),
            "tab" => Some(("Tab", 9, None)),
            "escape" => Some(("Escape", 27, None)),
            "delete" => Some(("Delete", 46, None)),
            "left" => Some(("ArrowLeft", 37, None)),
            "up" => Some(("ArrowUp", 38, None)),
            "right" => Some(("ArrowRight", 39, None)),
            "down" => Some(("ArrowDown", 40, None)),
            _ => None,
        };
        if let Some((key, vk, text)) = named {
            let mut down =
                json!({ "type": "keyDown", "key": key, "code": key, "windowsVirtualKeyCode": vk });
            if let Some(t) = text {
                down["text"] = json!(t);
            }
            self.input("Input.dispatchKeyEvent", down);
            self.input(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": vk }),
            );
        } else if let Some(ch) = k.key_char.as_deref().filter(|c| !c.is_empty()) {
            self.input("Input.insertText", json!({ "text": ch }));
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let focused = self.view.focused.clone();
        let current = self
            .view
            .tabs
            .iter()
            .find(|(id, ..)| Some(id) == focused.as_ref())
            .cloned();
        let label = current
            .as_ref()
            .map(|(_, title, url)| {
                if title.is_empty() {
                    url.clone()
                } else {
                    title.clone()
                }
            })
            .unwrap_or_else(|| "No tab".into());
        let tabs = self.view.tabs.clone();
        let (host, session) = (self.host.clone(), self.session(cx));
        let picker = super::kit::quiet_select("browser-tab", clip(&label, 40)).dropdown_menu(
            move |mut menu, _, _| {
                for (id, title, url) in &tabs {
                    let (host, session, id2) = (host.clone(), session.clone(), id.clone());
                    let text = if title.is_empty() {
                        url.clone()
                    } else {
                        title.clone()
                    };
                    menu = menu.item(
                        PopupMenuItem::new(clip(&text, 60))
                            .checked(focused.as_deref() == Some(id))
                            .on_click(move |_, _, _| {
                                if let Some(s) = &session {
                                    host.focus(s, &id2);
                                }
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
            .when(!self.view.tabs.is_empty(), |el| el.child(picker))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .children(current.map(|(_, _, url)| url)),
            )
            .when(self.frame.is_some(), |el| {
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
        let para = |t: String| {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(t)
        };
        let mut col = v_flex().p_4().gap_3();
        if let Some(e) = &self.view.chrome_error {
            col = col
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("The browser can't start"),
                )
                .child(para(e.clone()));
        } else {
            col = col
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("No pages open"))
                .child(para(
                    "The agent has browser tools in this session: ask it to open or check a page (for example your dev server on localhost). Its tabs show here live."
                        .into(),
                ));
        }
        col = col.child(div().text_xs().text_color(theme.muted_foreground).child(
            if self.view.attached {
                "Browser tools: on for this session"
            } else {
                "Browser tools: connecting…"
            },
        ));
        col.into_any_element()
    }

    fn render_live(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(f) = &self.frame else {
            return v_flex()
                .p_4()
                .items_center()
                .child(Spinner::new().small().color(theme.muted_foreground))
                .into_any_element();
        };
        let ratio = if f.size.1 > 0 {
            f.size.0 as f32 / f.size.1 as f32
        } else {
            16. / 10.
        };
        let image = f.image.clone();
        let bounds = self.view_bounds.clone();
        let control = self.control;
        let css = if f.size.0 > 0 {
            f.device.0 / f.size.0 as f32
        } else {
            1.
        };
        let size_label = format!(
            "{}×{}",
            (f.size.0 as f32 * css).round(),
            (f.size.1 as f32 * css).round()
        );
        let frame = div()
            .id("browser-frame")
            .w_full()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(if control { theme.primary } else { theme.border })
            // The canvas paints the frame and records where, so clicks map
            // onto exactly the pixels shown.
            .child(
                canvas(
                    move |b, _, _| {
                        bounds.set(b);
                        b
                    },
                    move |_, b, window, _| {
                        let _ = window.paint_image(b, b, Corners::default(), image, 0, false);
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
                            if let Some((x, y)) = this.page_point(ev.position) {
                                for t in ["mousePressed", "mouseReleased"] {
                                    this.input("Input.dispatchMouseEvent", json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }));
                                }
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll(cx.listener(|this, ev: &ScrollEvent, window, cx| {
                        let Some((x, y)) = this.page_point(ev.position) else { return };
                        let d = ev.delta.pixel_delta(window.line_height());
                        // gpui: positive y scrolls up; CDP: positive deltaY scrolls down.
                        this.input("Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": -f32::from(d.x), "deltaY": -f32::from(d.y) }));
                        cx.stop_propagation();
                    }))
            });
        v_flex()
            .p_2()
            .gap_2()
            .child(frame)
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(size_label)
                    .when(self.view.tabs.len() > 1, |el| {
                        el.child(format!("{} tabs", self.view.tabs.len()))
                    })
                    .child(div().flex_1())
                    .child(if control {
                        "Control on: the page gets your clicks and keys"
                    } else {
                        "Watching the agent's browser"
                    }),
            )
            .into_any_element()
    }
}

/// Base64 JPEG → BGRA RenderImage.
fn decode(b64: &str) -> Option<(Arc<RenderImage>, (u32, u32))> {
    let jpeg = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let mut rgba = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let size = rgba.dimensions();
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])),
        size,
    ))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for old in self.stale.drain(..) {
            let _ = window.drop_image(old);
        }
        let body = if self.streaming.is_some() {
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
