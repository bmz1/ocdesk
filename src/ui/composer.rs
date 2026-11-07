//! The prompt box: Enter sends, Shift+Enter inserts a newline, and the send
//! button turns into Stop while the selected session is busy.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::{Connection, Store};

pub struct Composer {
    input: Entity<TextareaState>,
    controller: Entity<Controller>,
    store: Entity<Store>,
    _subs: Vec<Subscription>,
}

impl Composer {
    pub fn new(
        controller: Entity<Controller>,
        store: Entity<Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask opencode anything…")
                .auto_grow(1, 10)
                .submit_on_enter(true)
        });
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } = ev
                {
                    this.submit(window, cx);
                }
                cx.notify();
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            input,
            controller,
            store,
            _subs: subs,
        }
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn busy(&self, cx: &App) -> bool {
        let s = self.store.read(cx);
        s.selected.as_deref().is_some_and(|id| s.is_busy(id))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy(cx) {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.input.update(cx, |i, cx| i.set_value("", window, cx));
        self.controller.update(cx, |c, cx| c.send_prompt(text, cx));
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let busy = self.busy(cx);
        let connected = self.store.read(cx).connection() == Connection::Connected;
        let empty = self.input.read(cx).value().trim().is_empty();

        let action = if busy {
            Button::new("stop")
                .icon(IconName::Square)
                .small()
                .rounded(px(999.))
                .primary()
                .tooltip("Stop")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.update(cx, |c, cx| c.abort(cx));
                }))
        } else {
            Button::new("send")
                .icon(IconName::ArrowUp)
                .small()
                .rounded(px(999.))
                .primary()
                .tooltip("Send (Enter)")
                .disabled(empty || !connected)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        v_flex()
            .w_full()
            .rounded_3xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_sm()
            .px_3()
            .pt_2()
            .pb_2()
            .gap_1()
            .child(Textarea::new(&self.input).appearance(false))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .px_1()
                            .child(if busy {
                                "Working…"
                            } else {
                                "Enter to send · Shift+Enter for a new line"
                            }),
                    )
                    .child(action),
            )
    }
}
