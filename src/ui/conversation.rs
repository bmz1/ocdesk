//! The main column: header, virtualized message list, status line, composer.
//! With no session selected it shows a centered Codex-style start screen.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::events::RunState;
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};
use crate::ui::composer::Composer;
use crate::ui::message::{COLUMN_MAX_W, render_message};

pub struct ConversationView {
    store: Entity<Store>,
    controller: Entity<Controller>,
    composer: Entity<Composer>,
    scroller: Entity<MessageScrollerState>,
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
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let subs = vec![cx.subscribe_in(&store, window, Self::on_store_event)];
        Composer::focus(&composer, window, cx);
        Self {
            store,
            controller,
            composer,
            scroller,
            _subs: subs,
        }
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
            StoreEvent::SelectionChanged => {
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
                Composer::focus(&self.composer, window, cx);
            }
            StoreEvent::MessagesReset { session_id } if is_selected(session_id) => {
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
            }
            StoreEvent::MessageInserted { session_id, index } if is_selected(session_id) => {
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.splice(i..i, 1, cx);
                });
            }
            StoreEvent::MessageRemoved { session_id, index } if is_selected(session_id) => {
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.splice(i..i + 1, 0, cx);
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
        let dir = self.controller.read(cx).working_dir().clone();
        let project = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.display().to_string());
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
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::Folder).xsmall())
                            .child(project),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN_MAX_W))
                    .child(self.composer.clone()),
            )
    }

    fn render_status(&self, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let s = self.store.read(cx);
        let sid = s.selected.as_ref()?;
        if let Some(RunState::Retry { attempt, message }) = s.status.get(sid) {
            return Some(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::TriangleAlert).xsmall())
                    .child(format!("Retrying (attempt {attempt}): {message}"))
                    .into_any_element(),
            );
        }
        let err = s.errors.get(sid)?;
        if err == "Aborted" {
            return None;
        }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let notice = store.notice.clone();
        let title = store
            .selected
            .as_ref()
            .and_then(|id| store.session(id))
            .map(|s| s.title.clone());
        let has_selection = store.selected.is_some();

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
            let list = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                let s = store.read(cx);
                let row = s
                    .selected_conversation()
                    .and_then(|c| c.messages.get(ix))
                    .map(|m| render_message(m, cx))
                    .unwrap_or_else(|| div().into_any_element());
                h_flex()
                    .w_full()
                    .justify_center()
                    .px_6()
                    .py_3()
                    .child(div().w_full().max_w(px(COLUMN_MAX_W)).child(row))
            })
            .with_bottom_fade(theme.background);
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(list))
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
                    .child(div().truncate().child(title.unwrap_or_default())),
            )
            .children(notice_bar)
            .child(div().flex_1().min_h_0().child(body))
    }
}
