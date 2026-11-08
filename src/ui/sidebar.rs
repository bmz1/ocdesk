//! Folders with their sessions (Codex-style projects), plus the connection
//! indicator. Each folder is an opencode location; sessions are grouped by
//! the directory they are pinned to.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::SessionInfo;
use crate::controller::Controller;
use crate::store::{Connection, Store};
use crate::ui::kit::RowStatus;
use crate::ui::{folder_name, relative_time};

/// Default, minimum and maximum sidebar width (logical px).
pub const SIDEBAR_W: f32 = 272.;
pub const SIDEBAR_MIN_W: f32 = 200.;
pub const SIDEBAR_MAX_W: f32 = 480.;

/// Sessions shown per folder before "Show more".
const FOLDER_PAGE: usize = 12;

pub struct Sidebar {
    store: Entity<Store>,
    controller: Entity<Controller>,
    focus: FocusHandle,
    /// Show only sessions that need you or have news (Cmd/Ctrl+Shift+U).
    activity: bool,
    /// Folders showing all their sessions.
    show_all: std::collections::HashSet<PathBuf>,
    _subs: Vec<Subscription>,
}

/// A session's status for its row.
pub fn row_status(store: &Store, id: &str) -> RowStatus {
    let waiting = store.permissions.get(id).is_some_and(|p| !p.is_empty())
        || store.forms.get(id).is_some_and(|f| !f.is_empty())
        || store
            .children_of(id)
            .iter()
            .any(|c| store.permissions.get(&c.id).is_some_and(|p| !p.is_empty()));
    if waiting {
        RowStatus::NeedsYou
    } else if store.is_busy(id) {
        RowStatus::Running
    } else if store.errors.contains_key(id) {
        RowStatus::Failed
    } else if store.unread.contains(id) {
        RowStatus::Unread
    } else {
        RowStatus::None
    }
}

/// "Today", "Yesterday", "This week", "Earlier" for an update time.
fn age_bucket(ms: u64, now: u64) -> &'static str {
    const DAY: u64 = 86_400_000;
    let age = now.saturating_sub(ms);
    if age < DAY {
        "Today"
    } else if age < 2 * DAY {
        "Yesterday"
    } else if age < 7 * DAY {
        "This week"
    } else {
        "Earlier"
    }
}

impl Sidebar {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        // Relative times ("5m") age without store changes.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            store,
            controller,
            focus: cx.focus_handle(),
            activity: false,
            show_all: Default::default(),
            _subs: subs,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub fn toggle_activity(&mut self, cx: &mut Context<Self>) {
        self.activity = !self.activity;
        cx.notify();
    }

    /// Up/Down move through sessions, Enter goes to the prompt, Delete
    /// archives, P pins.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.platform || m.alt {
            return;
        }
        let selected = self.store.read(cx).selected.clone();
        match ev.keystroke.key.as_str() {
            "down" => self.controller.update(cx, |c, cx| c.select_adjacent(1, cx)),
            "up" => self
                .controller
                .update(cx, |c, cx| c.select_adjacent(-1, cx)),
            "enter" => window.dispatch_action(Box::new(super::keys::FocusComposer), cx),
            "delete" | "backspace" => {
                if let Some(id) = selected {
                    Self::archive(self.controller.clone(), id, window, cx);
                }
            }
            "p" => {
                if let Some(id) = selected {
                    self.controller.update(cx, |c, cx| c.toggle_pin(id, cx));
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Archive with a 5 s Undo toast.
    fn archive(controller: Entity<Controller>, id: String, window: &mut Window, cx: &mut App) {
        let title = controller
            .read(cx)
            .view_store()
            .read(cx)
            .session(&id)
            .map(SessionInfo::display_title)
            .unwrap_or_default();
        controller.update(cx, |c, cx| c.set_archived(id.clone(), true, cx));
        let undo: super::kit::WindowAction = std::rc::Rc::new(move |_, cx| {
            let id = id.clone();
            controller.update(cx, |c, cx| c.set_archived(id, false, cx));
        });
        super::kit::toast(
            window,
            cx,
            format!("Archived “{title}”"),
            Some(("Undo", undo)),
        );
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

    pub fn open_rename(
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
            .child(super::kit::monogram(&folder_name(folder), 16., cx))
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
                            .tooltip("New session in this project")
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
                                let (c2, f2) = (controller.clone(), f_menu.clone());
                                let path = f_menu.display().to_string();
                                menu.item(PopupMenuItem::new("New session").on_click(
                                    move |_, _, cx| {
                                        let f = f2.clone();
                                        c2.update(cx, |c, cx| c.new_session(Some(f), cx));
                                    },
                                ))
                                .item(PopupMenuItem::new("Copy path").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
                                }))
                                .separator()
                                .item(
                                    PopupMenuItem::new("Remove from sidebar").on_click(
                                        move |_, _, cx| {
                                            let f = f.clone();
                                            c.update(cx, |c, cx| c.remove_folder(f, cx));
                                        },
                                    ),
                                )
                            }),
                    ),
            );

        let mut col = v_flex().gap_0p5().child(header);
        if !collapsed {
            let pinned = store.preferences.pinned.clone();
            let mut sessions: Vec<SessionInfo> = store.sessions_in(folder).cloned().collect();
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
            // Pinned first (in pin order), then by recency as listed.
            sessions.sort_by_key(|s| pinned.iter().position(|p| *p == s.id).unwrap_or(usize::MAX));
            let total = sessions.len();
            let show_all = self.show_all.contains(folder);
            let now = crate::store::now_ms();
            let mut bucket: Option<&'static str> = None;
            let many_buckets = sessions
                .iter()
                .filter(|s| !pinned.contains(&s.id))
                .map(|s| age_bucket(s.updated, now))
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1;
            for (j, s) in sessions.iter().enumerate() {
                if !show_all && j >= FOLDER_PAGE {
                    break;
                }
                let is_pinned = pinned.contains(&s.id);
                let b = if is_pinned {
                    "Pinned"
                } else {
                    age_bucket(s.updated, now)
                };
                if bucket != Some(b) && (many_buckets || is_pinned || bucket == Some("Pinned")) {
                    col = col.child(
                        div()
                            .pl_8()
                            .pt_1()
                            .text_xs()
                            .text_color(theme.muted_foreground.opacity(0.8))
                            .child(b),
                    );
                }
                bucket = Some(b);
                col = col.child(self.render_session(ix * 10_000 + j, s, false, false, cx));
            }
            if total > FOLDER_PAGE {
                let f = folder.clone();
                col = col.child(
                    div()
                        .id(SharedString::from(format!("show-more-{ix}")))
                        .pl_8()
                        .py_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground))
                        .child(if show_all {
                            "Show less".to_string()
                        } else {
                            format!("Show {} more", total - FOLDER_PAGE)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.show_all.remove(&f) {
                                this.show_all.insert(f.clone());
                            }
                            cx.notify();
                        })),
                );
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
                    col = col.child(self.render_session(key, s, true, false, cx));
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
        with_project: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let id = s.id.clone();
        let is_selected = store.selected.as_deref() == Some(&id);
        let status = row_status(store, &id);
        let pinned = store.preferences.pinned.contains(&id);
        let group: SharedString = format!("session-row-{key}").into();
        let title = s.display_title();
        let project = with_project.then(|| {
            store
                .folder_of(&s.directory)
                .map(|f| folder_name(&f))
                .unwrap_or_default()
        });
        let controller = self.controller.clone();
        let (menu_id, menu_title) = (id.clone(), title.clone());
        let (pin_id, archive_id) = (id.clone(), id.clone());
        let status_label = status.label();
        h_flex()
            .id(SharedString::from(format!("session-{id}")))
            .group(group.clone())
            .flex_none()
            .h_8()
            .when(with_project, |el| el.pl_2())
            .when(!with_project && !nested, |el| el.pl_8())
            .when(nested, |el| el.pl_12())
            .pr_1()
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
            .when(status == RowStatus::Unread && !is_selected, |el| {
                el.font_weight(FontWeight::MEDIUM)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = id.clone();
                this.controller
                    .update(cx, |c, cx| c.select_session(Some(id), cx));
            }))
            .when_some(project.clone(), |el, p| {
                el.child(super::kit::monogram(&p, 16., cx))
            })
            .child(div().flex_1().min_w_0().truncate().child(title))
            // Trailing slot: status, else the age; actions replace the age
            // under the pointer so times stay aligned.
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .items_center()
                    .when(pinned, |el| {
                        el.child(
                            super::icons::pin()
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("status-{key}")))
                            .when(status != RowStatus::None, |el| {
                                el.tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(status_label)
                                        .build(window, cx)
                                })
                            })
                            .child(super::kit::status_slot(status, cx)),
                    )
                    .child(
                        div()
                            .w(px(28.))
                            .text_right()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(relative_time(s.updated)),
                    ),
            )
            .child(
                h_flex()
                    .absolute()
                    .right(px(4.))
                    .gap_0p5()
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(
                        Button::new(SharedString::from(format!("session-pin-{key}")))
                            .icon(if pinned {
                                super::icons::pin_off()
                            } else {
                                super::icons::pin()
                            })
                            .ghost()
                            .xsmall()
                            .tooltip(if pinned { "Unpin" } else { "Pin to top" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let id = pin_id.clone();
                                this.controller.update(cx, |c, cx| c.toggle_pin(id, cx));
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("session-archive-{key}")))
                            .icon(super::icons::archive())
                            .ghost()
                            .xsmall()
                            .tooltip("Archive (Undo for 5 s)")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                Self::archive(
                                    this.controller.clone(),
                                    archive_id.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("session-menu-{key}")))
                            .icon(IconName::Ellipsis)
                            .ghost()
                            .xsmall()
                            .dropdown_menu(move |menu, _, _| {
                                let (c1, id1, t1) =
                                    (controller.clone(), menu_id.clone(), menu_title.clone());
                                let (c2, id2, t2) =
                                    (controller.clone(), menu_id.clone(), menu_title.clone());
                                let (c3, id3) = (controller.clone(), menu_id.clone());
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
                                .item(PopupMenuItem::new("Fork").on_click(move |_, _, cx| {
                                    let id = id3.clone();
                                    c3.update(cx, |c, cx| {
                                        c.select_session(Some(id), cx);
                                        c.fork(None, cx);
                                    })
                                }))
                                .separator()
                                .item(
                                    PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
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
            .relative()
            .into_any_element()
    }

    /// The "Needs you" view: sessions across folders that wait on the user,
    /// failed, run, or have news.
    fn render_activity(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let mut rows: Vec<(RowStatus, SessionInfo)> = store
            .sessions
            .iter()
            .filter(|s| s.parent_id.is_none())
            .map(|s| (row_status(store, &s.id), s.clone()))
            .filter(|(st, _)| *st != RowStatus::None)
            .collect();
        let rank = |s: RowStatus| match s {
            RowStatus::NeedsYou => 0,
            RowStatus::Failed => 1,
            RowStatus::Unread => 2,
            RowStatus::Running => 3,
            RowStatus::None => 4,
        };
        rows.sort_by_key(|(st, s)| (rank(*st), std::cmp::Reverse(s.updated)));
        let mut col = v_flex().gap_0p5();
        if rows.is_empty() {
            col = col.child(
                div()
                    .px_2()
                    .py_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Nothing needs you right now."),
            );
        }
        for (i, (_, s)) in rows.iter().enumerate() {
            col = col.child(self.render_session(900_000 + i, s, false, true, cx));
        }
        col.into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let folders = self.store.read(cx).folders.clone();
        let activity = self.activity;
        let groups: Vec<AnyElement> = if activity {
            vec![self.render_activity(cx)]
        } else {
            folders
                .iter()
                .enumerate()
                .map(|(ix, f)| self.render_folder(ix, f, cx))
                .collect()
        };
        let empty = folders.is_empty();
        let needs_you = {
            let store = self.store.read(cx);
            store
                .sessions
                .iter()
                .filter(|s| row_status(store, &s.id) == RowStatus::NeedsYou)
                .count()
        };

        let nav_row = |id: &'static str, icon: Icon, label: &'static str| {
            h_flex()
                .id(id)
                .h_8()
                .px_2()
                .gap_2()
                .rounded_lg()
                .text_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent.opacity(0.6)))
                .child(icon.small())
                .child(label)
        };
        let overrides = self.store.read(cx).preferences.keybindings.clone();
        let shortcut = |action: &dyn Action, _: &Window, cx: &App| {
            div()
                .flex_1()
                .text_right()
                .text_xs()
                .text_color(theme.muted_foreground.opacity(0.8))
                .children(super::keys::shortcut_label(action, &overrides, cx))
        };

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key(ev, window, cx)))
            .child(
                v_flex()
                    .px_2()
                    .pt_3()
                    .pb_2()
                    // Traffic lights (macOS) sit above the first row; the
                    // band drags the window.
                    .child(
                        div()
                            .h(px(super::app_view::TITLEBAR_INSET_H))
                            .window_control_area(WindowControlArea::Drag),
                    )
                    .child(
                        nav_row("new-session", Icon::new(IconName::Plus), "New session")
                            .child(shortcut(&super::keys::NewSession, window, cx))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| c.new_session(None, cx));
                            })),
                    )
                    .child(
                        nav_row("search-sessions", Icon::new(IconName::Search), "Search")
                            .child(shortcut(&super::keys::CommandPalette, window, cx))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(super::keys::CommandPalette), cx)
                            }),
                    )
                    .child(
                        nav_row("activity", Icon::new(IconName::Inbox), "Needs you")
                            .when(activity, |el| el.bg(theme.sidebar_accent))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .justify_end()
                                    .when(needs_you > 0, |el| {
                                        el.child(
                                            div()
                                                .px_1p5()
                                                .rounded_full()
                                                .bg(theme.warning)
                                                .text_xs()
                                                .text_color(theme.background)
                                                .child(needs_you.to_string()),
                                        )
                                    }),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_activity(cx))),
                    ),
            )
            .child(
                h_flex()
                    .px_4()
                    .pt_2()
                    .pb_1()
                    .justify_between()
                    .items_center()
                    .child(super::kit::section_label(
                        if activity { "Needs you" } else { "Projects" },
                        cx,
                    ))
                    .child(
                        Button::new("add-folder")
                            .icon(IconName::Plus)
                            .ghost()
                            .xsmall()
                            .tooltip("Add project…")
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
                                "Add a project folder to start. Sessions are grouped by project.",
                            ),
                        )
                    }),
            )
            .child(
                v_flex().px_2().pb_1().child(
                    nav_row("open-settings", Icon::new(IconName::Settings), "Settings")
                        .child(shortcut(&super::keys::OpenSettings, window, cx))
                        .on_click(cx.listener(|this, _, window, cx| {
                            crate::ui::settings::open(this.controller.clone(), window, cx)
                        })),
                ),
            )
            .child(self.render_connection(cx))
    }
}
