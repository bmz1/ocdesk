//! Session list: new, select, rename, delete, plus the connection indicator.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::{Connection, Store};
use crate::ui::relative_time;

pub const SIDEBAR_W: f32 = 260.;

pub struct Sidebar {
    store: Entity<Store>,
    controller: Entity<Controller>,
    _subs: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            store,
            controller,
            _subs: subs,
        }
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let store = self.store.read(cx);
        let version = store.opencode_version.clone().unwrap_or_default();
        let (color, label, down) = match store.connection() {
            Connection::Starting => (theme.warning, "Starting opencode…".to_string(), None),
            Connection::Connecting => (theme.warning, "Connecting…".to_string(), None),
            Connection::Connected => (theme.success, format!("opencode {version}"), None),
            Connection::Reconnecting => (theme.warning, "Reconnecting…".to_string(), None),
            Connection::SidecarDown { message, .. } => {
                (theme.danger, "opencode stopped".to_string(), Some(message))
            }
        };
        v_flex()
            .gap_2()
            .px_3()
            .py_3()
            .border_t_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().size_2().rounded_full().bg(color))
                    .child(label),
            )
            .when_some(down, |el, message| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .line_clamp(3)
                        .child(message),
                )
                .child(
                    Button::new("restart-sidecar")
                        .label("Restart opencode")
                        .icon(IconName::RefreshCw)
                        .small()
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |c, cx| c.restart(cx));
                        })),
                )
            })
    }

    fn open_rename(
        controller: Entity<Controller>,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        // The dialog focuses its shell on open; move focus into the input on
        // the builder's first run only (see `WindowExt::open_dialog` docs).
        let initial_focus = std::cell::Cell::new(true);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            if initial_focus.replace(false) {
                input.update(cx, |i, cx| i.focus(window, cx));
            }
            let input = input.clone();
            let controller = controller.clone();
            let id = id.clone();
            alert
                .title("Rename session")
                .description(Input::new(&input))
                .confirm()
                .ok_text("Rename")
                .on_ok(move |_, _, cx| {
                    let title = input.read(cx).value().to_string();
                    let id = id.clone();
                    controller.update(cx, |c, cx| c.rename_session(id, title, cx));
                    true
                })
        });
    }

    fn open_delete(
        controller: Entity<Controller>,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.open_alert_dialog(cx, move |alert, _, _| {
            let controller = controller.clone();
            let id = id.clone();
            alert
                .title("Delete session?")
                .description(format!(
                    "“{title}” and its history will be deleted. This can't be undone."
                ))
                .confirm()
                .ok_text("Delete")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    controller.update(cx, |c, cx| c.delete_session(id, cx));
                    true
                })
        });
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let selected = store.selected.clone();

        let rows: Vec<AnyElement> = store
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let id = s.id.clone();
                let is_selected = selected.as_deref() == Some(&id);
                let busy = store.is_busy(&id);
                let group: SharedString = format!("session-row-{ix}").into();
                let title = if s.title.is_empty() {
                    "Untitled".to_string()
                } else {
                    s.title.clone()
                };
                let controller = self.controller.clone();
                let (menu_id, menu_title) = (id.clone(), title.clone());
                h_flex()
                    .id(SharedString::from(format!("session-{id}")))
                    .group(group.clone())
                    .flex_none()
                    .h_8()
                    .px_2()
                    .gap_2()
                    .rounded_lg()
                    .text_sm()
                    .cursor_pointer()
                    .when(is_selected, |el| {
                        el.bg(theme.sidebar_accent)
                            .text_color(theme.sidebar_accent_foreground)
                    })
                    .when(!is_selected, |el| {
                        el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.controller
                            .update(cx, |c, cx| c.select_session(Some(id), cx));
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(if busy {
                        Spinner::new()
                            .xsmall()
                            .color(theme.muted_foreground)
                            .into_any_element()
                    } else {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(relative_time(s.time.updated))
                            .into_any_element()
                    })
                    .child(
                        div().invisible().group_hover(group, |s| s.visible()).child(
                            Button::new(SharedString::from(format!("session-menu-{ix}")))
                                .icon(IconName::Ellipsis)
                                .ghost()
                                .xsmall()
                                .dropdown_menu(move |menu, _, _| {
                                    let (c1, id1, t1) =
                                        (controller.clone(), menu_id.clone(), menu_title.clone());
                                    let (c2, id2, t2) =
                                        (controller.clone(), menu_id.clone(), menu_title.clone());
                                    menu.item(PopupMenuItem::new("Rename").on_click(
                                        move |_, window, cx| {
                                            Self::open_rename(
                                                c1.clone(),
                                                id1.clone(),
                                                t1.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("Delete").on_click(
                                            move |_, window, cx| {
                                                Self::open_delete(
                                                    c2.clone(),
                                                    id2.clone(),
                                                    t2.clone(),
                                                    window,
                                                    cx,
                                                )
                                            },
                                        ),
                                    )
                                }),
                        ),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex().px_2().pt_3().pb_2().child(
                    h_flex()
                        .id("new-session")
                        .h_8()
                        .px_2()
                        .gap_2()
                        .rounded_lg()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |c, cx| c.new_session(cx));
                        }))
                        .child(Icon::new(IconName::Plus).small())
                        .child("New session"),
                ),
            )
            .child(
                div()
                    .px_4()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child("Sessions"),
            )
            .child(
                v_flex()
                    .id("session-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_0p5()
                    .children(rows),
            )
            .child(self.render_connection(cx))
    }
}
