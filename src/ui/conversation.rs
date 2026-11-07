//! The main column: header, virtualized message list, status line, composer.
//! With no session selected it shows a centered Codex-style start screen.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::{Connection, Store, StoreEvent};
use crate::ui::composer::Composer;
use crate::ui::folder_name;
use crate::ui::message::{COLUMN_MAX_W, render_message};
use crate::ui::prose::{self, ProseCache};

/// Below this window width the tasks drawer overlays the conversation.
const SPLIT_MIN_WINDOW_W: f32 = 1440.;

pub struct ConversationView {
    expanded_tools: std::collections::HashSet<String>,
    show_changes: bool,
    /// Free-text inputs for pending question forms, keyed (form, field).
    form_inputs: super::form_view::FormInputs,
    store: Entity<Store>,
    controller: Entity<Controller>,
    composer: Entity<Composer>,
    tasks: Entity<super::tasks::TasksDrawer>,
    tasks_open: bool,
    scroller: Entity<MessageScrollerState>,
    prose: Entity<ProseCache>,
    _subs: Vec<Subscription>,
}

impl ConversationView {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| Composer::new(controller.clone(), store.clone(), window, cx));
        // The store may already hold a selected conversation (demo mode).
        let rows = store
            .read(cx)
            .selected_conversation()
            .map_or(0, |c| c.messages.len());
        let scroller = cx.new(|cx| MessageScrollerState::new(rows, cx));
        let prose = cx.new(|_| ProseCache::new(store.clone(), scroller.clone()));
        let subs = vec![
            cx.subscribe_in(&store, window, Self::on_store_event),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Composer::focus(&composer, window, cx);
        let tasks =
            cx.new(|cx| super::tasks::TasksDrawer::new(store.clone(), controller.clone(), cx));
        Self {
            tasks,
            tasks_open: false,
            expanded_tools: Default::default(),
            show_changes: false,
            form_inputs: Default::default(),
            store,
            controller,
            composer,
            scroller,
            prose,
            _subs: subs,
        }
    }

    /// Create (or drop) the free-text inputs pending forms need, and keep
    /// typed text synced into the form's draft so required-field gating and
    /// submission see it.
    fn sync_form_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> super::form_view::FormInputs {
        // Collect what's needed up front: `cx` is mutably borrowed while
        // entities are created.
        let wanted_fields: Vec<(String, crate::api::model::FormField)> = {
            let store = self.store.read(cx);
            let mut out = Vec::new();
            if let Some(sid) = store.selected.clone()
                && let Some(forms) = store.forms.get(&sid)
            {
                for form in forms {
                    for field in form.fields.iter() {
                        if super::form_view::field_needs_input(field) {
                            out.push((form.id.clone(), field.clone()));
                        }
                    }
                }
            }
            out
        };
        let mut wanted = super::form_view::FormInputs::new();
        for (fid, field) in wanted_fields {
            let k = (fid.clone(), field.key.clone());
            if let Some(state) = self.form_inputs.get(&k) {
                wanted.insert(k.clone(), state.clone());
                continue;
            }
            let placeholder = field
                .placeholder
                .clone()
                .unwrap_or_else(|| "Your answer…".into());
            let state = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).placeholder(placeholder)
            });
            // Typed text becomes the field's answer immediately.
            let (form_id, key) = (fid.clone(), field.key.clone());
            let sub = cx.subscribe_in(
                &state,
                window,
                move |this, input, ev: &gpui_kit::component::input::InputEvent, _, cx| {
                    if !matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                        return;
                    }
                    let text = input.read(cx).value().to_string();
                    let (form_id, key) = (form_id.clone(), key.clone());
                    this.controller.update(cx, |c, cx| {
                        c.set_form_answer(form_id, key, serde_json::Value::String(text), cx);
                    });
                },
            );
            self._subs.push(sub);
            self.form_inputs.insert(k.clone(), state.clone());
            wanted.insert(k, state);
        }
        // Drop inputs of fields that are gone (form answered, cancelled, or
        // snapshot without it).
        self.form_inputs.retain(|k, _| wanted.contains_key(k));
        wanted
    }

    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Composer::focus(&self.composer, window, cx);
    }

    pub fn toggle_tasks(&mut self, cx: &mut Context<Self>) {
        if self.store.read(cx).selected.is_none() {
            return;
        }
        self.tasks_open = !self.tasks_open;
        if self.tasks_open {
            self.tasks
                .update(cx, |t, cx| t.show(super::tasks::TaskView::List, cx));
        }
        cx.notify();
    }

    pub fn open_task(&mut self, view: super::tasks::TaskView, cx: &mut Context<Self>) {
        self.tasks_open = true;
        self.tasks.update(cx, |t, cx| t.show(view, cx));
        cx.notify();
    }

    /// Switch between the conversation and the latest turn's changes.
    pub fn toggle_changes(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        self.show_changes = !self.show_changes;
        if self.show_changes {
            self.controller.update(cx, |c, cx| c.refresh_diff(sid, cx));
        }
        cx.notify();
    }

    fn row_count(&self, cx: &App) -> usize {
        self.store
            .read(cx)
            .selected_conversation()
            .map_or(0, |c| c.messages.len())
    }

    fn on_store_event(
        &mut self,
        _: &Entity<Store>,
        ev: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.store.read(cx).selected.clone();
        let is_selected = |sid: &String| selected.as_ref() == Some(sid);
        match ev {
            StoreEvent::SelectionChanged | StoreEvent::FoldersChanged => {
                self.show_changes = false;
                self.expanded_tools.clear();
                if matches!(ev, StoreEvent::SelectionChanged) {
                    self.prose.update(cx, |p, _| p.clear());
                }
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
                Composer::focus(&self.composer, window, cx);
            }
            StoreEvent::MessagesReset { session_id } if is_selected(session_id) => {
                self.prose.update(cx, |p, _| p.invalidate());
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
            }
            StoreEvent::MessageInserted { session_id, index } if is_selected(session_id) => {
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.splice(i..i, 1, cx);
                });
            }
            StoreEvent::MessageChanged { session_id, index } if is_selected(session_id) => {
                // Only the streaming row is re-measured.
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.remeasure_items(i..i + 1, cx);
                });
            }
            _ => {}
        }
        cx.notify();
    }

    fn render_start(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = self.render_connection_banner(cx);
        let theme = cx.theme();
        let store = self.store.read(cx);
        let folders = store.folders.clone();
        let current = store.target_folder();
        let controller = self.controller.clone();

        let chooser = if folders.is_empty() {
            Button::new("start-add-folder")
                .label("Add a folder to start")
                .icon(IconName::Folder)
                .primary()
                .on_click(cx.listener(|this, _, window, cx| {
                    crate::ui::add_folder(this.controller.clone(), window, cx);
                }))
                .into_any_element()
        } else {
            Button::new("folder-chooser")
                .label(current.as_deref().map(folder_name).unwrap_or_default())
                .icon(IconName::Folder)
                .ghost()
                .small()
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for f in &folders {
                        let (c, f2) = (controller.clone(), f.clone());
                        menu = menu.item(
                            PopupMenuItem::new(folder_name(f))
                                .checked(current.as_ref() == Some(f))
                                .on_click(move |_, _, cx| {
                                    let f = f2.clone();
                                    c.update(cx, |c, cx| c.new_session(Some(f), cx));
                                }),
                        );
                    }
                    let c = controller.clone();
                    menu.separator()
                        .item(
                            PopupMenuItem::new("Add folder…").on_click(move |_, window, cx| {
                                crate::ui::add_folder(c.clone(), window, cx);
                            }),
                        )
                })
                .into_any_element()
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .gap_6()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("What should we build?"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(chooser),
                    ),
            )
            .when(store.target_folder().is_some(), |el| {
                el.child(
                    v_flex()
                        .w_full()
                        .max_w(px(COLUMN_MAX_W))
                        .gap_2()
                        .children(banner)
                        .child(self.composer.clone()),
                )
            })
    }

    /// Why prompts can't be sent right now, above the composer. The
    /// sidebar footer carries the details and the Restart button.
    fn render_connection_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (color, icon, text, restart) = match self.store.read(cx).connection() {
            Connection::Connected => return None,
            // Startup is brief and the sidebar already says so.
            Connection::Starting | Connection::Connecting => return None,
            Connection::Reconnecting => (
                theme.warning,
                IconName::TriangleAlert,
                "Reconnecting to opencode… prompts are paused.",
                false,
            ),
            Connection::SidecarDown { .. } => (
                theme.danger,
                IconName::CircleAlert,
                "opencode isn't running, so prompts can't be sent.",
                true,
            ),
        };
        Some(
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(color)
                .child(Icon::new(icon).xsmall())
                .child(div().flex_1().min_w_0().child(text))
                .when(restart, |el| {
                    el.child(
                        Button::new("banner-restart")
                            .label("Restart")
                            .icon(IconName::RefreshCw)
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| c.restart(cx));
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    /// In place of the message list while history is loading or failed.
    fn render_placeholder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let store = self.store.read(cx);
        let sid = store.selected.clone()?;
        let has_messages = store
            .selected_conversation()
            .is_some_and(|c| !c.messages.is_empty());
        if has_messages {
            return None;
        }
        let centered = || {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .px_6()
                .text_sm()
                .text_color(theme.muted_foreground)
        };
        if let Some(err) = store.load_errors.get(&sid) {
            let connected = store.connection() == Connection::Connected;
            return Some(
                centered()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_color(theme.danger)
                            .child(Icon::new(IconName::CircleAlert).xsmall())
                            .child("Couldn't load this conversation"),
                    )
                    .child(
                        div()
                            .max_w(px(COLUMN_MAX_W))
                            .text_xs()
                            .text_center()
                            .child(err.clone()),
                    )
                    .child(
                        Button::new("retry-load")
                            .label("Retry")
                            .icon(IconName::RefreshCw)
                            .small()
                            .outline()
                            .disabled(!connected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let sid = sid.clone();
                                this.controller.update(cx, |c, cx| c.load_messages(sid, cx));
                            })),
                    )
                    .into_any_element(),
            );
        }
        store
            .selected_is_loading()
            .then(|| centered().child("Loading conversation…").into_any_element())
    }

    fn render_status(&self, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let s = self.store.read(cx);
        let sid = s.selected.as_ref()?;
        if let Some(message) = s.retry.get(sid) {
            return Some(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::TriangleAlert).xsmall())
                    .child(format!("Retrying: {message}"))
                    .into_any_element(),
            );
        }
        let err = s.errors.get(sid)?;
        Some(
            h_flex()
                .gap_1p5()
                .text_sm()
                .text_color(theme.danger)
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(err.clone())
                .into_any_element(),
        )
    }
}

impl Render for ConversationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let notice = store.notice.clone();
        let session = store.selected.as_ref().and_then(|id| store.session(id));
        let title = session.map(|s| s.display_title());
        let folder = session.map(|s| folder_name(&s.directory));
        let has_selection = store.selected.is_some();
        let store_running = store
            .selected
            .as_ref()
            .map_or(0, |id| store.running_tasks(id));
        let form_inputs = self.sync_form_inputs(window, cx);

        let notice_bar = notice.map(|n| {
            h_flex()
                .mx_4()
                .mt_2()
                .px_3()
                .py_2()
                .gap_2()
                .rounded_lg()
                .bg(theme.secondary)
                .text_sm()
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(div().flex_1().child(n))
                .child(
                    Button::new("dismiss-notice")
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| {
                                s.notice = None;
                                cx.notify();
                            });
                        })),
                )
        });

        let body = if !has_selection {
            self.render_start(cx).into_any_element()
        } else {
            let store = self.store.clone();
            let expanded = self.expanded_tools.clone();
            let view = cx.entity().downgrade();
            let toggle: super::tool_card::ToggleTool = std::rc::Rc::new(move |key, cx| {
                let _ = view.update(cx, |this, cx| {
                    let index = this.store.read(cx).selected_conversation().and_then(|c| {
                        c.messages
                            .iter()
                            .position(|m| key.starts_with(&format!("{}/", m.id())))
                    });
                    if !this.expanded_tools.remove(&key) {
                        this.expanded_tools.insert(key);
                    }
                    if let Some(i) = index {
                        this.scroller
                            .update(cx, |s, cx| s.remeasure_items(i..i + 1, cx));
                    }
                    cx.notify();
                });
            });
            let cache = self.prose.clone();
            let view = cx.entity().downgrade();
            let open_task: super::tasks::OpenTask = std::rc::Rc::new(move |task, cx| {
                let _ = view.update(cx, |this, cx| this.open_task(task, cx));
            });
            let list = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                prose::sync_row(&store, &cache, ix, cx);
                let s = store.read(cx);
                let row = s
                    .selected_conversation()
                    .and_then(|c| c.messages.get(ix))
                    .map(|m| render_message(m, &cache, &expanded, toggle.clone(), &open_task, cx))
                    .unwrap_or_else(|| div().into_any_element());
                h_flex()
                    .w_full()
                    .justify_center()
                    .px_6()
                    .py_3()
                    .child(div().w_full().max_w(px(COLUMN_MAX_W)).child(row))
            })
            .with_bottom_fade(theme.background);
            let placeholder = self.render_placeholder(cx);
            let content = if let Some(p) = placeholder.filter(|_| !self.show_changes) {
                p
            } else if self.show_changes {
                div()
                    .id("changes-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .p_6()
                    .child(super::changes::render_changes(
                        self.store.read(cx),
                        self.controller.clone(),
                        cx,
                    ))
                    .into_any_element()
            } else {
                list.into_any_element()
            };
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(content))
                .child(
                    h_flex()
                        .w_full()
                        .justify_center()
                        .px_6()
                        .pb_4()
                        .pt_1()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(px(COLUMN_MAX_W))
                                .gap_2()
                                .child(super::form_view::render_forms(
                                    self.store.read(cx),
                                    &form_inputs,
                                    self.controller.clone(),
                                    cx,
                                ))
                                .child(super::permission::render_permissions(
                                    self.store.read(cx),
                                    self.controller.clone(),
                                    cx,
                                ))
                                .children(self.render_connection_banner(cx))
                                .children(self.render_status(cx))
                                .child(self.composer.clone()),
                        ),
                )
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .h_11()
                    .flex_none()
                    .px_4()
                    .items_center()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .gap_1p5()
                    .child(
                        Button::new("toggle-sidebar")
                            .icon(IconName::PanelLeft)
                            .ghost()
                            .small()
                            .tooltip_with_action(
                                "Toggle sidebar",
                                &super::keys::ToggleSidebar,
                                None,
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(super::keys::ToggleSidebar), cx)
                            }),
                    )
                    .when_some(folder, |el, f| {
                        el.child(div().text_color(theme.muted_foreground).child(f))
                            .child(div().text_color(theme.muted_foreground).child("/"))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(title.unwrap_or_default()),
                    )
                    .when(has_selection, |el| {
                        let running = store_running;
                        el.child(
                            Button::new("toggle-tasks")
                                .icon(super::icons::workflow())
                                .label(if running > 0 {
                                    format!("Tasks · {running}")
                                } else {
                                    "Tasks".to_string()
                                })
                                .ghost()
                                .small()
                                .selected(self.tasks_open)
                                .tooltip_with_action(
                                    "Background tasks: subagents and shells",
                                    &super::keys::ToggleTasks,
                                    None,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_tasks(cx))),
                        )
                        .child(
                            Button::new("toggle-changes")
                                .label(if self.show_changes {
                                    "Conversation"
                                } else {
                                    "Changes"
                                })
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_changes(cx))),
                        )
                    }),
            )
            .children(notice_bar)
            .child({
                // Side by side when there's room for the column and the
                // drawer; otherwise the drawer floats over the conversation.
                let show_tasks = self.tasks_open && has_selection;
                let split = window.viewport_size().width >= px(SPLIT_MIN_WINDOW_W);
                h_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_w_0().h_full().child(body))
                    .when(show_tasks && split, |el| el.child(self.tasks.clone()))
                    .when(show_tasks && !split, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right_0()
                                .shadow_lg()
                                .child(self.tasks.clone()),
                        )
                    })
            })
    }
}
