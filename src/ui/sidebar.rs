//! Folders with their sessions (Codex-style projects), plus the connection
//! indicator. Each folder is an opencode location; sessions are grouped by
//! the directory they are pinned to.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::SessionInfo;
use crate::controller::Controller;
use crate::store::{Connection, Store};
use crate::ui::{folder_name, relative_time};

/// Default, minimum and maximum sidebar width (logical px).
pub const SIDEBAR_W: f32 = 272.;
pub const SIDEBAR_MIN_W: f32 = 200.;
pub const SIDEBAR_MAX_W: f32 = 480.;

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
            Connection::SidecarDown {
                message,
                last_lines,
            } => (
                theme.danger,
                "opencode stopped".to_string(),
                Some((message, last_lines)),
            ),
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
            .when_some(down, |el, (message, last_lines)| {
                // Everything needed for a bug report: the error and the
                // sidecar's last output lines.
                let details = if last_lines.is_empty() {
                    message.clone()
                } else {
                    format!("{message}\n\n{}", last_lines.join("\n"))
                };
                el.child(
                    div()
                        .id("sidecar-error")
                        .w_full()
                        .min_w_0()
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(message),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
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
                        .child(
                            Button::new("copy-sidecar-error")
                                .label("Copy details")
                                .icon(IconName::Copy)
                                .small()
                                .ghost()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        details.clone(),
                                    ));
                                }),
                        ),
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
            let (input, controller, id) = (input.clone(), controller.clone(), id.clone());
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
            let (controller, id) = (controller.clone(), id.clone());
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

    fn render_folder(&self, ix: usize, folder: &PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let collapsed = store.collapsed.contains(folder);
        let is_current = store.selected.is_none() && store.current_folder.as_ref() == Some(folder);
        let group: SharedString = format!("folder-{ix}").into();
        let controller = self.controller.clone();
        let (f_new, f_toggle, f_menu) = (folder.clone(), folder.clone(), folder.clone());

        let header = h_flex()
            .id(SharedString::from(format!("folder-header-{ix}")))
            .group(group.clone())
            .flex_none()
            .h_8()
            .px_2()
            .gap_1p5()
            .rounded_lg()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(is_current, |el| el.bg(theme.sidebar_accent.opacity(0.6)))
            .hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
            .tooltip({
                let path: SharedString = folder.display().to_string().into();
                move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(path.clone()).build(window, cx)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let f = f_toggle.clone();
                this.controller.update(cx, |c, cx| c.toggle_folder(f, cx));
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .child(
                Icon::new(IconName::Folder)
                    .small()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(folder_name(folder)),
            )
            .child(
                h_flex()
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(
                        Button::new(SharedString::from(format!("folder-new-{ix}")))
                            .icon(IconName::Plus)
                            .ghost()
                            .xsmall()
                            .tooltip("New session in this folder")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let f = f_new.clone();
                                this.controller
                                    .update(cx, |c, cx| c.new_session(Some(f), cx));
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("folder-menu-{ix}")))
                            .icon(IconName::Ellipsis)
                            .ghost()
                            .xsmall()
                            .dropdown_menu(move |menu, _, _| {
                                let (c, f) = (controller.clone(), f_menu.clone());
                                menu.item(PopupMenuItem::new("Remove from sidebar").on_click(
                                    move |_, _, cx| {
                                        let f = f.clone();
                                        c.update(cx, |c, cx| c.remove_folder(f, cx));
                                    },
                                ))
                            }),
                    ),
            );

        let mut col = v_flex().gap_0p5().child(header);
        if !collapsed {
            let sessions: Vec<SessionInfo> = store.sessions_in(folder).cloned().collect();
            if sessions.is_empty() {
                col = col.child(
                    div()
                        .pl_8()
                        .py_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("No sessions yet"),
                );
            }
            for (j, s) in sessions.iter().enumerate() {
                col = col.child(self.render_session(ix * 10_000 + j, s, false, cx));
            }
            // Worktrees of this repository, each with its own sessions.
            let worktrees = self
                .store
                .read(cx)
                .worktrees
                .get(folder)
                .cloned()
                .unwrap_or_default();
            for (w, dir) in worktrees.iter().enumerate() {
                col = col.child(self.render_worktree(ix, w, folder, dir, cx));
                let sessions: Vec<SessionInfo> =
                    self.store.read(cx).sessions_in(dir).cloned().collect();
                for (j, s) in sessions.iter().enumerate() {
                    let key = ix * 10_000 + (w + 1) * 1_000 + j;
                    col = col.child(self.render_session(key, s, true, cx));
                }
            }
        }
        col.into_any_element()
    }

    /// A worktree checkout under its repository: `⎇ feature-x · branch`,
    /// with "+" (new session there) and Remove on hover.
    fn render_worktree(
        &self,
        ix: usize,
        w: usize,
        folder: &Path,
        dir: &Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let label = crate::ui::git::checkout_label(store, dir);
        let is_current = store.selected.is_none() && store.current_folder.as_deref() == Some(dir);
        let group: SharedString = format!("worktree-{ix}-{w}").into();
        let controller = self.controller.clone();
        let (f_menu, d_menu, d_new) = (folder.to_path_buf(), dir.to_path_buf(), dir.to_path_buf());
        h_flex()
            .id(SharedString::from(format!("worktree-row-{ix}-{w}")))
            .group(group.clone())
            .flex_none()
            .h_7()
            .pl_6()
            .pr_2()
            .gap_1p5()
            .rounded_lg()
            .text_xs()
            .text_color(theme.muted_foreground)
            .cursor_pointer()
            .when(is_current, |el| el.bg(theme.sidebar_accent.opacity(0.6)))
            .hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
            .tooltip({
                let path: SharedString = dir.display().to_string().into();
                move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(path.clone()).build(window, cx)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let d = d_new.clone();
                this.controller
                    .update(cx, |c, cx| c.new_session(Some(d), cx));
            }))
            .child(crate::ui::icons::git_branch().xsmall())
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                h_flex()
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(crate::ui::git::new_session_button(
                        SharedString::from(format!("worktree-new-{ix}-{w}")),
                        controller.clone(),
                        dir.to_path_buf(),
                    ))
                    .child(
                        Button::new(SharedString::from(format!("worktree-menu-{ix}-{w}")))
                            .icon(IconName::Ellipsis)
                            .ghost()
                            .xsmall()
                            .dropdown_menu(move |menu, _, _| {
                                let (c, f, d) =
                                    (controller.clone(), f_menu.clone(), d_menu.clone());
                                menu.item(PopupMenuItem::new("Remove worktree…").on_click(
                                    move |_, window, cx| {
                                        crate::ui::git::remove_worktree(
                                            c.clone(),
                                            f.clone(),
                                            d.clone(),
                                            window,
                                            cx,
                                        )
                                    },
                                ))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_session(
        &self,
        key: usize,
        s: &SessionInfo,
        nested: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let id = s.id.clone();
        let is_selected = store.selected.as_deref() == Some(&id);
        let unread = store.unread.contains(&id);
        let busy = store.is_busy(&id);
        let waiting = store.permissions.get(&id).map_or(0, Vec::len);
        let group: SharedString = format!("session-row-{key}").into();
        let title = s.display_title();
        let controller = self.controller.clone();
        let (menu_id, menu_title) = (id.clone(), title.clone());
        h_flex()
            .id(SharedString::from(format!("session-{id}")))
            .group(group.clone())
            .flex_none()
            .h_8()
            .when(!nested, |el| el.pl_8())
            .when(nested, |el| el.pl_12())
            .pr_2()
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
            .when(unread, |el| {
                el.child(div().text_color(theme.primary).child("●"))
            })
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(if waiting > 0 {
                div()
                    .text_xs()
                    .text_color(theme.warning)
                    .child(format!("! {waiting}"))
                    .into_any_element()
            } else if busy {
                Spinner::new()
                    .xsmall()
                    .color(theme.muted_foreground)
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .group_hover(group.clone(), |s| s.invisible())
                    .child(relative_time(s.updated))
                    .into_any_element()
            })
            .child(
                div().invisible().group_hover(group, |s| s.visible()).child(
                    Button::new(SharedString::from(format!("session-menu-{key}")))
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
                                PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                                    Self::open_delete(
                                        c2.clone(),
                                        id2.clone(),
                                        t2.clone(),
                                        window,
                                        cx,
                                    )
                                }),
                            )
                        }),
                ),
            )
            .into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let folders = self.store.read(cx).folders.clone();
        let groups: Vec<AnyElement> = folders
            .iter()
            .enumerate()
            .map(|(ix, f)| self.render_folder(ix, f, cx))
            .collect();
        let empty = folders.is_empty();

        let nav_row = |id: &'static str, icon: IconName, label: &'static str| {
            h_flex()
                .id(id)
                .h_8()
                .px_2()
                .gap_2()
                .rounded_lg()
                .text_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
                .child(Icon::new(icon).small())
                .child(label)
        };

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                v_flex().px_2().pt_3().pb_2().child(nav_row("new-session", IconName::Plus, "New session").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.new_session(None, cx));
                    }),
                )),
            )
            .child(
                h_flex()
                    .px_4()
                    .pt_2()
                    .pb_1()
                    .justify_between()
                    .items_center()
                    .child(
                        div().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child("Folders"),
                    )
                    .child(
                        Button::new("add-folder")
                            .icon(IconName::Plus)
                            .ghost()
                            .xsmall()
                            .tooltip("Add folder…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                crate::ui::add_folder(this.controller.clone(), window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("folder-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_1()
                    .children(groups)
                    .when(empty, |el| {
                        el.child(
                            v_flex().px_2().py_4().gap_2().text_sm().text_color(theme.muted_foreground).child(
                                "Add a project folder to start. Sessions are grouped by folder.",
                            ),
                        )
                    }),
            )
            .child(Button::new("open-settings").label("Settings").ghost().small().on_click(cx.listener(|this,_,window,cx|crate::ui::settings::open(this.controller.clone(),window,cx))))
            .child(self.render_connection(cx))
    }
}
