//! The main column: header, virtualized message list, status line, composer.
//! With no session selected it shows a centered Codex-style start screen.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::{Store, StoreEvent};
use crate::ui::composer::Composer;
use crate::ui::folder_name;
use crate::ui::message::{COLUMN_MAX_W, render_message};
use crate::ui::prose::{self, ProseCache};

pub struct ConversationView {
    expanded_tools: std::collections::HashSet<String>,
    show_changes: bool,
    /// Free-text inputs for pending question forms, keyed (form, field).
    form_inputs: super::form_view::FormInputs,
    store: Entity<Store>,
    controller: Entity<Controller>,
    composer: Entity<Composer>,
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
        Self {
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
                    div()
                        .w_full()
                        .max_w(px(COLUMN_MAX_W))
                        .child(self.composer.clone()),
                )
            })
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
            let list = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                prose::sync_row(&store, &cache, ix, cx);
                let s = store.read(cx);
                let row = s
                    .selected_conversation()
                    .and_then(|c| c.messages.get(ix))
                    .map(|m| render_message(m, &cache, &expanded, toggle.clone(), cx))
                    .unwrap_or_else(|| div().into_any_element());
                h_flex()
                    .w_full()
                    .justify_center()
                    .px_6()
                    .py_3()
                    .child(div().w_full().max_w(px(COLUMN_MAX_W)).child(row))
            })
            .with_bottom_fade(theme.background);
            let content = if self.show_changes {
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
                        el.child(
                            Button::new("toggle-changes")
                                .label(if self.show_changes {
                                    "Conversation"
                                } else {
                                    "Changes"
                                })
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_changes = !this.show_changes;
                                    if this.show_changes
                                        && let Some(sid) = this.store.read(cx).selected.clone()
                                    {
                                        this.controller.update(cx, |c, cx| c.refresh_diff(sid, cx));
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .children(notice_bar)
            .child(div().flex_1().min_h_0().child(body))
    }
}
