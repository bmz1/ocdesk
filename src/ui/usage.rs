use crate::{controller::Controller, store::Store};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

pub fn meter(store: &Store, controller: Entity<Controller>, cx: &App) -> AnyElement {
    let Some(sid) = store.selected.clone() else {
        return div().into_any_element();
    };
    let label = if let Some(r) = store.usage.get(&sid) {
        let context = match (r.context, r.context_limit) {
            (Some(n), Some(limit)) if limit > 0 => {
                format!(" · context ~{:.0}%", n as f64 / limit as f64 * 100.)
            }
            (Some(n), _) => format!(" · context {n}"),
            _ => String::new(),
        };
        format!("{} tokens · ${:.4}{context}", r.tokens.total(), r.cost)
    } else if store.usage_loading.contains(&sid) {
        "Usage loading…".into()
    } else {
        "Usage details".into()
    };
    let button = Button::new("usage-meter")
        .label(label)
        .ghost()
        .xsmall()
        .on_click({
            let sid = sid.clone();
            move |_, window, cx| open(sid.clone(), controller.clone(), window, cx)
        });
    let mut row = h_flex().gap_2().items_center().child(button);
    if let Some(r) = store.usage.get(&sid)
        && let (Some(n), Some(limit)) = (r.context, r.context_limit)
        && limit > 0
    {
        let fraction = (n as f32 / limit as f32).clamp(0., 1.);
        let color = if fraction > 0.9 {
            cx.theme().danger
        } else if fraction > 0.75 {
            cx.theme().warning
        } else {
            cx.theme().primary
        };
        row = row.child(
            div()
                .w(px(70.))
                .h(px(4.))
                .rounded_md()
                .bg(cx.theme().secondary)
                .child(div().h_full().w(relative(fraction)).rounded_md().bg(color)),
        );
    }
    row.into_any_element()
}
struct UsageDetails {
    sid: String,
    controller: Entity<Controller>,
    store: Entity<Store>,
    _sub: Subscription,
}
fn open(sid: String, controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
    let store = controller.read(cx).view_store();
    let view = cx.new(|cx| {
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        UsageDetails {
            sid,
            controller,
            store,
            _sub: sub,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Usage and context")
            .w(px(650.))
            .child(view.clone())
    });
}
impl Render for UsageDetails {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.store.read(cx);
        let report = s.usage.get(&self.sid).cloned();
        let error = s.usage_errors.get(&self.sid).cloned();
        let loading = s.usage_loading.contains(&self.sid);
        let mut col = v_flex().gap_3();
        if let Some(r) = &report {
            col = col.child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "Session total: {} tokens · ${:.6}",
                r.tokens.total(),
                r.cost
            )));
            col = col.child(div().text_sm().child(format!(
                "Input {} · output {} · reasoning {} (included in output)",
                r.tokens.input, r.tokens.output, r.tokens.reasoning
            )));
            col = col.child(div().text_sm().child(format!(
                "Cache read {} · cache write {}",
                r.tokens.cache_read, r.tokens.cache_write
            )));
            col =
                col.child(
                    div().text_sm().child(match (r.context, r.context_limit) {
                        (Some(n), Some(limit)) => {
                            format!("Last request context: {n} / {limit} tokens (estimate)")
                        }
                        (Some(n), _) => format!(
                            "Last request context: {n} tokens (estimate; model limit unavailable)"
                        ),
                        _ => {
                            "Last request context unavailable until an assistant request completes."
                                .into()
                        }
                    }),
                );
            col = col.child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Conversation by model"),
            );
            for (model, usage) in &r.models {
                col = col.child(div().text_sm().child(format!(
                    "{model}: {} tokens · ${:.6} · cache read {} / write {}",
                    usage.tokens.total(),
                    usage.cost,
                    usage.tokens.cache_read,
                    usage.tokens.cache_write
                )));
            }
            col=col.child(div().text_xs().text_color(cx.theme().muted_foreground).child("Session totals can include title generation and other overhead. Model rows cover assistant messages in the conversation. Reasoning tokens are a subset of output; cache tokens are counted once."));
            col = col.child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Provider usage limits"),
            );
            if let Some(limits) = &r.limits {
                col = col.child(
                    div()
                        .text_xs()
                        .child(serde_json::to_string_pretty(limits).unwrap_or_default()),
                );
            } else {
                col = col.child(
                    div().text_sm().child(
                        r.limits_error
                            .as_ref()
                            .map(|e| format!("Could not load limits: {e}"))
                            .unwrap_or_else(|| {
                                "This provider does not report usage limits.".into()
                            }),
                    ),
                );
            }
        }
        if let Some(error) = &error {
            col = col.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        let c = self.controller.clone();
        let id = self.sid.clone();
        col = col.child(
            h_flex().justify_end().child(
                Button::new("refresh-usage")
                    .label(if loading {
                        "Loading…"
                    } else {
                        "Refresh usage"
                    })
                    .outline()
                    .small()
                    .on_click(move |_, _, cx| {
                        c.update(cx, |c, cx| c.refresh_usage(id.clone(), cx));
                    }),
            ),
        );

        div()
            .id("usage-scroll")
            .max_h(px(540.))
            .overflow_y_scroll()
            .child(col)
    }
}
