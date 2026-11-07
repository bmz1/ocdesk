//! Root layout: sidebar | conversation, app-wide shortcuts, and persistence
//! of the window placement and sidebar.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::{self, Controller};
use crate::settings::{Settings, WindowPlacement};
use crate::store::Store;
use crate::ui::conversation::ConversationView;
use crate::ui::frame_stats::FrameStats;
use crate::ui::keys;
use crate::ui::sidebar::{SIDEBAR_MAX_W, SIDEBAR_MIN_W, SIDEBAR_W, Sidebar};

/// Reverse-DNS id shared by the window class, `.desktop` file and `.app`.
pub const APP_ID: &str = "io.github.bmz1.ocdesk";

/// Drag payload of the sidebar's resize handle.
struct SidebarResize;

impl Render for SidebarResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct AppView {
    /// Fallback focus target, so window-level shortcuts still dispatch to
    /// this view when nothing inside it has focus.
    focus: FocusHandle,
    controller: Entity<Controller>,
    sidebar: Entity<Sidebar>,
    conversation: Entity<ConversationView>,
    sidebar_width: Pixels,
    sidebar_hidden: bool,
    first_frame_logged: bool,
    frames: FrameStats,
    _subs: Vec<Subscription>,
}

/// Window options for the saved placement, or a centered default.
pub fn window_options(settings: &Settings, cx: &App) -> WindowOptions {
    let default = || Bounds::centered(None, size(px(1200.), px(800.)), cx);
    let window_bounds = match settings.window.filter(WindowPlacement::is_sane) {
        Some(p) => {
            let bounds = Bounds::new(point(px(p.x), px(p.y)), size(px(p.width), px(p.height)));
            // A display may have been unplugged since: only reuse the
            // position when the title bar area is on some display.
            let grab = point(bounds.origin.x + px(40.), bounds.origin.y + px(10.));
            let visible = cx.displays().iter().any(|d| d.bounds().contains(&grab));
            let bounds = if visible {
                bounds
            } else {
                Bounds::centered(None, bounds.size, cx)
            };
            if p.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            }
        }
        None => WindowBounds::Windowed(default()),
    };
    WindowOptions {
        window_bounds: Some(window_bounds),
        window_min_size: Some(size(px(WindowPlacement::MIN_W), px(WindowPlacement::MIN_H))),
        // Matches the desktop entry (StartupWMClass) and the macOS bundle id.
        app_id: Some(APP_ID.into()),
        titlebar: Some(TitlebarOptions {
            title: Some("ocdesk".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl AppView {
    pub fn new(
        rt: tokio::runtime::Handle,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Dark by default; "light" or "system" (follow the OS) on request.
        let theme = std::env::var("OCDESK_THEME")
            .ok()
            .or_else(|| settings.theme.clone());

        match theme.as_deref() {
            Some("light") => Theme::change(ThemeMode::Light, Some(window), cx),
            Some("system") => Theme::sync_system_appearance(Some(window), cx),
            _ => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
        let sidebar_width = px(settings
            .sidebar_width
            .unwrap_or(SIDEBAR_W)
            .clamp(SIDEBAR_MIN_W, SIDEBAR_MAX_W));
        let sidebar_hidden = settings.sidebar_hidden;
        let store = cx.new(|_| Store::default());
        let initial = controller::initial_folder(!settings.folders.is_empty());
        let controller = cx.new(|cx| Controller::new(rt, store.clone(), settings, initial, cx));
        if let Ok(p) = std::env::var("OCDESK_PROBE_PROMPT") {
            controller.update(cx, |c, cx| c.probe_on_connect(p, cx));
        }
        let notification_controller = controller.downgrade();
        let notification_window = window.window_handle();
        cx.on_system_notification_response(move |response, cx| {
            let Some(sid) = response.tag.strip_prefix("ocdesk:").map(str::to_string) else {
                return;
            };
            cx.activate(true);
            let _ = notification_window.update(cx, |_, window, cx| {
                window.activate_window();
                let _ = notification_controller.update(cx, |c, cx| {
                    c.set_window_active(true, cx);
                    c.select_session(Some(sid), cx);
                });
            });
        });
        let sidebar = cx.new(|cx| Sidebar::new(store.clone(), controller.clone(), cx));
        let conversation =
            cx.new(|cx| ConversationView::new(store.clone(), controller.clone(), window, cx));
        controller.update(cx, |c, cx| {
            c.set_window_active(window.is_window_active(), cx)
        });
        let mut last_theme = controller.read(cx).preferences().theme;
        let mut last_badge = usize::MAX;
        let mut subs = vec![
            cx.observe_window_activation(window, |this, window, cx| {
                this.controller.update(cx, |c, cx| {
                    c.set_window_active(window.is_window_active(), cx)
                })
            }),
            cx.observe_in(&store, window, move |_, store, window, cx| {
                let s = store.read(cx);
                let theme = s.preferences.theme.clone();
                let count = s.unread.len();
                if theme != last_theme {
                    last_theme = theme.clone();
                    match theme.as_deref() {
                        Some("light") => Theme::change(ThemeMode::Light, Some(window), cx),
                        Some("system") => Theme::sync_system_appearance(Some(window), cx),
                        _ => Theme::change(ThemeMode::Dark, Some(window), cx),
                    }
                }
                if count != last_badge {
                    last_badge = count;
                    crate::notifications::badge(count);
                    window.set_window_title(&if count == 0 {
                        "ocdesk".into()
                    } else {
                        format!("ocdesk ({count} unread)")
                    });
                }
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.remember_placement(window, cx)
            }),
            // Debounced saves may still be pending; write them before exit.
            cx.on_app_quit(|this, cx| {
                this.controller.update(cx, |c, _| c.flush_settings());
                async {}
            }),
        ];
        subs.push(cx.observe_window_appearance(window, |this, window, cx| {
            if this.controller.read(cx).preferences().theme.as_deref() == Some("system") {
                Theme::sync_system_appearance(Some(window), cx);
            }
        }));
        Self {
            focus: cx.focus_handle(),
            controller,
            sidebar,
            conversation,
            sidebar_width,
            sidebar_hidden,
            first_frame_logged: false,
            frames: FrameStats::new(),
            _subs: subs,
        }
    }

    fn remember_placement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // For maximized/fullscreen windows these are the restore bounds, so
        // un-maximizing after a restart lands where the user left it.
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(b) => (b, false),
            WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, true),
        };
        let placement = WindowPlacement {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            maximized,
        };
        if placement.is_sane() {
            self.controller.update(cx, |c, cx| {
                c.update_settings(cx, |s| s.window = Some(placement))
            });
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        let hidden = self.sidebar_hidden;
        self.controller.update(cx, |c, cx| {
            c.update_settings(cx, |s| s.sidebar_hidden = hidden)
        });
        cx.notify();
    }

    fn resize_sidebar(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let w = px(f32::from(x).clamp(SIDEBAR_MIN_W, SIDEBAR_MAX_W).round());
        if w == self.sidebar_width {
            return;
        }
        self.sidebar_width = w;
        let saved: f32 = w.into();
        self.controller.update(cx, |c, cx| {
            c.update_settings(cx, |s| s.sidebar_width = Some(saved))
        });
        cx.notify();
    }

    fn render_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("sidebar-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left(self.sidebar_width - px(3.))
            .w(px(6.))
            .cursor_col_resize()
            .child(
                // The visible 1px border, highlighted while hovered.
                div().mx_auto().w(px(1.)).h_full().bg(theme.sidebar_border),
            )
            .hover(|s| s.bg(theme.sidebar_border.opacity(0.6)))
            .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| SidebarResize))
            .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                // Double-click restores the default width.
                if e.click_count() == 2 {
                    this.resize_sidebar(px(SIDEBAR_W), cx);
                }
            }))
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames.begin();
        if window.focused(cx).is_none() {
            window.focus(&self.focus, cx);
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            if let Some(t) = crate::STARTED.get() {
                tracing::info!(ms = t.elapsed().as_millis() as u64, "first frame rendered");
            }
        }
        let show_sidebar = !self.sidebar_hidden;
        h_flex()
            .id("app")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &keys::NewSession, _, cx| {
                this.controller.update(cx, |c, cx| c.new_session(None, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::AddFolder, window, cx| {
                crate::ui::add_folder(this.controller.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, _: &keys::ToggleSidebar, _, cx| {
                this.toggle_sidebar(cx);
            }))
            .on_action(cx.listener(|this, _: &keys::PreviousSession, _, cx| {
                this.controller
                    .update(cx, |c, cx| c.select_adjacent(-1, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::NextSession, _, cx| {
                this.controller.update(cx, |c, cx| c.select_adjacent(1, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::FocusComposer, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.focus_composer(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::StopTurn, _, cx| {
                this.controller.update(cx, |c, cx| c.abort_if_busy(cx));
            }))
            .on_action(cx.listener(|this, _: &keys::ToggleChanges, _, cx| {
                this.conversation.update(cx, |c, cx| c.toggle_changes(cx));
            }))
            .on_action(cx.listener(|this, _: &keys::ToggleTasks, _, cx| {
                this.conversation.update(cx, |c, cx| c.toggle_tasks(cx));
            }))
            .on_action(cx.listener(|this, _: &keys::OpenSettings, window, cx| {
                crate::ui::settings::open(this.controller.clone(), window, cx)
            }))
            .on_action(|_: &keys::ShowShortcuts, window, cx| keys::show_shortcuts(window, cx))
            .on_action(|_: &keys::Quit, _, cx| cx.quit())
            .on_drag_move(
                cx.listener(|this, e: &DragMoveEvent<SidebarResize>, _, cx| {
                    this.resize_sidebar(e.event.position.x - e.bounds.left(), cx);
                }),
            )
            .when(show_sidebar, |el| {
                el.child(
                    div()
                        .w(self.sidebar_width)
                        .h_full()
                        .flex_none()
                        .child(self.sidebar.clone()),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.conversation.clone()),
            )
            .when(show_sidebar, |el| el.child(self.render_resize_handle(cx)))
            .children(self.frames.end_marker())
    }
}
