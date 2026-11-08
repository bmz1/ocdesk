//! Root layout: sidebar | conversation, app-wide shortcuts, and persistence
//! of the window placement and sidebar.

use gpui_kit::component::{ActiveTheme as _, Theme, h_flex};
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

/// Room the macOS traffic lights take at the window's top-left with a
/// transparent title bar: the sidebar's first row starts below it, and the
/// conversation header indents by it while the sidebar is hidden.
pub const TITLEBAR_INSET_H: f32 = if cfg!(target_os = "macos") { 30. } else { 0. };
pub const TITLEBAR_INSET_W: f32 = if cfg!(target_os = "macos") { 72. } else { 0. };

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
        // macOS: content under a transparent title bar, traffic lights
        // inset into the sidebar's top row (see `TITLEBAR_INSET`).
        titlebar: Some(TitlebarOptions {
            title: Some("ocdesk".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: cfg!(target_os = "macos").then(|| point(px(14.), px(15.))),
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

        crate::ui::theme::apply(theme.as_deref(), window, cx);
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
                let active = window.is_window_active();
                this.controller
                    .update(cx, |c, cx| c.set_window_active(active, cx));
                // Files may have been edited elsewhere while we were away.
                if active {
                    let tabs = this.conversation.read(cx).files().clone();
                    tabs.update(cx, |t, cx| t.revalidate_all(window, cx));
                }
            }),
            cx.observe_in(&store, window, move |_, store, window, cx| {
                let s = store.read(cx);
                let theme = s.preferences.theme.clone();
                let count = s.unread.len();
                if theme != last_theme {
                    last_theme = theme.clone();
                    crate::ui::theme::apply(theme.as_deref(), window, cx);
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
        // Closing the window with unsaved editor tabs asks first.
        let conversation_for_close = conversation.downgrade();
        let me = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let dirty = conversation_for_close
                .upgrade()
                .map(|c| c.read(cx).files().read(cx).dirty_files())
                .unwrap_or_default();
            if dirty.is_empty() {
                return true;
            }
            let _ = me.update(cx, |this, cx| {
                this.confirm_discard(window, cx, |_, cx| cx.quit())
            });
            false
        });
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

    /// Run `then` now, or after the user agreed to drop unsaved editor tabs.
    fn confirm_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        let dirty = self.conversation.read(cx).files().read(cx).dirty_files();
        if dirty.is_empty() {
            then(window, cx);
            return;
        }
        let detail = dirty.join("\n");
        let rx = window.prompt(
            PromptLevel::Warning,
            &format!("Discard unsaved changes in {} file(s)?", dirty.len()),
            Some(&detail),
            &["Discard and Quit", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |_, cx| {
            if rx.await == Ok(0) {
                let _ = cx.update(|window, cx| then(window, cx));
            }
        })
        .detach();
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

    /// F6 / Shift+F6: sidebar → conversation → prompt → right panel,
    /// skipping hidden areas, wrapping around.
    fn focus_area(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        #[derive(Clone, Copy, PartialEq)]
        enum Area {
            Sidebar,
            Transcript,
            Composer,
            Panel,
        }
        let mut areas = Vec::new();
        if !self.sidebar_hidden {
            areas.push(Area::Sidebar);
        }
        areas.push(Area::Transcript);
        areas.push(Area::Composer);
        if self.conversation.read(cx).panel_open(cx) {
            areas.push(Area::Panel);
        }
        let current = if self
            .sidebar
            .read(cx)
            .focus_handle()
            .contains_focused(window, cx)
        {
            Some(Area::Sidebar)
        } else {
            self.conversation
                .read(cx)
                .focused_area(window, cx)
                .map(|a| match a {
                    crate::ui::conversation::Area::Transcript => Area::Transcript,
                    crate::ui::conversation::Area::Composer => Area::Composer,
                    crate::ui::conversation::Area::Panel => Area::Panel,
                })
        };
        let n = areas.len() as isize;
        let at = current
            .and_then(|c| areas.iter().position(|a| *a == c))
            .map_or(if delta > 0 { -1 } else { 0 }, |i| i as isize);
        let next = areas[((at + delta).rem_euclid(n)) as usize];
        match next {
            Area::Sidebar => self.sidebar.update(cx, |s, cx| s.focus(window, cx)),
            Area::Transcript => self.conversation.update(cx, |c, cx| {
                c.focus_area(crate::ui::conversation::Area::Transcript, window, cx)
            }),
            Area::Composer => self.conversation.update(cx, |c, cx| {
                c.focus_area(crate::ui::conversation::Area::Composer, window, cx)
            }),
            Area::Panel => self.conversation.update(cx, |c, cx| {
                c.focus_area(crate::ui::conversation::Area::Panel, window, cx)
            }),
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
            .on_action(cx.listener(|this, _: &keys::ToggleChanges, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.toggle_changes(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::ToggleTasks, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.toggle_tasks(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::OpenSettings, window, cx| {
                crate::ui::settings::open(this.controller.clone(), window, cx)
            }))
            .on_action(|_: &keys::ShowShortcuts, window, cx| keys::show_shortcuts(window, cx))
            .on_action(cx.listener(|this, _: &keys::CommandPalette, window, cx| {
                crate::ui::palette::open(
                    this.controller.clone(),
                    crate::ui::palette::Mode::All,
                    window,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &keys::ChooseModel, window, cx| {
                crate::ui::palette::open(
                    this.controller.clone(),
                    crate::ui::palette::Mode::Models,
                    window,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &keys::FindInThread, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.open_find(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::CycleDensity, window, cx| {
                let msg = this.controller.update(cx, |c, cx| c.cycle_density(cx));
                crate::ui::kit::toast(window, cx, msg, None);
            }))
            .on_action(
                cx.listener(|this, _: &keys::CyclePermissionMode, window, cx| {
                    let msg = this
                        .controller
                        .update(cx, |c, cx| c.cycle_permission_mode(cx));
                    crate::ui::kit::toast(window, cx, msg, None);
                }),
            )
            .on_action(cx.listener(|this, _: &keys::CycleEffort, window, cx| {
                let msg = this.controller.update(cx, |c, cx| c.cycle_effort(cx));
                crate::ui::kit::toast(window, cx, msg, None);
            }))
            .on_action(cx.listener(|this, _: &keys::NextSessionTab, _, cx| {
                this.controller.update(cx, |c, cx| c.select_adjacent(1, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::PreviousSessionTab, _, cx| {
                this.controller
                    .update(cx, |c, cx| c.select_adjacent(-1, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::CopyLastReply, window, cx| {
                let text = this.controller.read(cx).last_reply(cx);
                let msg = match text {
                    Some(t) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                        "Copied the last reply"
                    }
                    None => "No reply to copy",
                };
                crate::ui::kit::toast(window, cx, msg, None);
            }))
            .on_action(cx.listener(|this, _: &keys::RenameSession, window, cx| {
                let store = this.controller.read(cx).view_store();
                let s = store.read(cx);
                if let Some(session) = s.selected.as_ref().and_then(|id| s.session(id)) {
                    let (id, title) = (session.id.clone(), session.display_title());
                    crate::ui::sidebar::Sidebar::open_rename(
                        this.controller.clone(),
                        id,
                        title,
                        window,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &keys::ShowActivity, _, cx| {
                if this.sidebar_hidden {
                    this.toggle_sidebar(cx);
                }
                this.sidebar.update(cx, |s, cx| s.toggle_activity(cx));
            }))
            .on_action(cx.listener(|this, _: &keys::ToggleTerminal, window, cx| {
                this.conversation.update(cx, |c, cx| {
                    c.toggle_panel(crate::settings::PanelTab::Terminal, window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &keys::FocusNextArea, window, cx| {
                this.focus_area(1, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keys::FocusPreviousArea, window, cx| {
                    this.focus_area(-1, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &keys::ToggleFiles, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.toggle_files(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::GoToFile, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                crate::ui::goto_file::open(this.controller.clone(), tabs, window, cx);
            }))
            .on_action(cx.listener(|this, a: &keys::OpenFile, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                tabs.update(cx, |t, cx| t.open(&a.path, a.line, window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::SaveFile, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                tabs.update(cx, |t, cx| t.save_active(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::CloseTab, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                tabs.update(cx, |t, cx| t.close_active(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::NextTab, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                tabs.update(cx, |t, cx| t.cycle(1, window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::PreviousTab, window, cx| {
                let tabs = this.conversation.read(cx).files().clone();
                tabs.update(cx, |t, cx| t.cycle(-1, window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::ShowChat, window, cx| {
                this.conversation
                    .update(cx, |c, cx| c.show_chat(window, cx));
            }))
            .on_action(cx.listener(|this, _: &keys::Quit, window, cx| {
                this.confirm_discard(window, cx, |_, cx| cx.quit());
            }))
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
