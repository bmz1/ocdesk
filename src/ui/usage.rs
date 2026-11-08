use crate::ui::kit::section_label;
use crate::ui::message::format_tokens;
use crate::{controller::Controller, store::Store};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, Theme, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

struct UsageDetails {
    sid: String,
    controller: Entity<Controller>,
    store: Entity<Store>,
    _sub: Subscription,
}
pub fn open(sid: String, controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
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
        let theme = cx.theme().clone();
        let s = self.store.read(cx);
        let report = s.usage.get(&self.sid).cloned();
        let error = s.usage_errors.get(&self.sid).cloned();
        let loading = s.usage_loading.contains(&self.sid);
        let mut col = v_flex().gap_5();

        if let Some(r) = &report {
            // Headline numbers.
            let fraction = match (r.context, r.context_limit) {
                (Some(n), Some(l)) if l > 0 => Some(n as f32 / l as f32),
                _ => None,
            };
            let context_value = match (r.context, r.context_limit) {
                (Some(n), Some(l)) => format!("{} / {}", format_tokens(n), format_tokens(l)),
                (Some(n), None) => format_tokens(n),
                _ => "—".into(),
            };
            let context_tile = tile(
                "Context",
                context_value,
                fraction.map(|f| format!("{:.0}% of window, last request", f * 100.)),
                &theme,
            )
            .when_some(fraction, |el, f| {
                el.child(bar(f, context_color(f, &theme), &theme))
            });
            col = col.child(
                h_flex()
                    .gap_2()
                    .child(tile(
                        "Tokens",
                        format_tokens(r.tokens.total()),
                        Some(group(r.tokens.total())),
                        &theme,
                    ))
                    .child(tile("Cost", money(r.cost), None, &theme))
                    .child(context_tile),
            );

            // Token breakdown.
            let t = &r.tokens;
            col = col.child(
                v_flex()
                    .gap_2()
                    .child(section_label("Breakdown", cx))
                    .child(
                        v_flex()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme.border)
                            .child(kv("Input", t.input, None, &theme, false))
                            .child(kv("Output", t.output, None, &theme, true))
                            .child(kv(
                                "Reasoning",
                                t.reasoning,
                                Some("part of output"),
                                &theme,
                                true,
                            ))
                            .child(kv("Cache read", t.cache_read, None, &theme, true))
                            .child(kv("Cache write", t.cache_write, None, &theme, true)),
                    ),
            );

            // Per model.
            if !r.models.is_empty() {
                let total = r
                    .models
                    .values()
                    .map(|m| m.tokens.total())
                    .sum::<u64>()
                    .max(1);
                let mut rows = v_flex().rounded_lg().border_1().border_color(theme.border);
                for (i, (model, usage)) in r.models.iter().enumerate() {
                    let (provider, name) = model.split_once('/').unwrap_or(("", model));
                    let share = usage.tokens.total() as f32 / total as f32;
                    rows = rows.child(
                        v_flex()
                            .px_3()
                            .py_2()
                            .gap_1p5()
                            .when(i > 0, |el| el.border_t_1().border_color(theme.border))
                            .child(
                                h_flex()
                                    .gap_3()
                                    .justify_between()
                                    .child(
                                        v_flex()
                                            .min_w_0()
                                            .child(
                                                div().text_sm().truncate().child(name.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(provider.to_string()),
                                            ),
                                    )
                                    .child(
                                        v_flex()
                                            .items_end()
                                            .flex_none()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .child(format_tokens(usage.tokens.total())),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!(
                                                        "{} · {} cached",
                                                        money(usage.cost),
                                                        format_tokens(usage.tokens.cache_read)
                                                    )),
                                            ),
                                    ),
                            )
                            .child(bar(share, theme.primary.opacity(0.7), &theme)),
                    );
                }
                col = col.child(
                    v_flex()
                        .gap_2()
                        .child(section_label("By model", cx))
                        .child(rows),
                );
            }

            // Provider limits.
            let limits: AnyElement = if let Some(limits) = &r.limits {
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.secondary.opacity(0.5))
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(serde_json::to_string_pretty(limits).unwrap_or_default())
                    .into_any_element()
            } else {
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        r.limits_error
                            .as_ref()
                            .map(|e| format!("Could not load limits: {e}"))
                            .unwrap_or_else(|| "Not reported by this provider.".into()),
                    )
                    .into_any_element()
            };
            col = col.child(
                v_flex()
                    .gap_2()
                    .child(section_label("Provider limits", cx))
                    .child(limits),
            );
        } else if loading {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading usage…"),
            );
        }
        if let Some(error) = &error {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        let c = self.controller.clone();
        let id = self.sid.clone();
        col = col.child(
            h_flex()
                .gap_4()
                .items_end()
                .justify_between()
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(
                            "Session totals include title generation and other overhead; \
                             model rows cover assistant messages only. Context is an estimate.",
                        ),
                )
                .child(
                    Button::new("refresh-usage")
                        .icon(IconName::RefreshCw)
                        .label(if loading { "Loading…" } else { "Refresh" })
                        .ghost()
                        .small()
                        .on_click(move |_, _, cx| {
                            c.update(cx, |c, cx| c.refresh_usage(id.clone(), cx));
                        }),
                ),
        );

        div()
            .id("usage-scroll")
            .max_h(px(560.))
            .overflow_y_scroll()
            .child(col)
    }
}

/// A headline stat: label, big value, optional muted sub-line.
fn tile(label: &str, value: String, sub: Option<String>, theme: &Theme) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .p_3()
        .rounded_lg()
        .bg(theme.secondary.opacity(0.5))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(value),
        )
        .when_some(sub, |el, sub| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(sub),
            )
        })
}

/// One row of the breakdown table: label (+ note), short count, exact count.
fn kv(label: &str, n: u64, note: Option<&str>, theme: &Theme, divider: bool) -> Div {
    h_flex()
        .px_3()
        .py_2()
        .gap_2()
        .text_sm()
        .when(divider, |el| el.border_t_1().border_color(theme.border))
        .child(label.to_string())
        .when_some(note, |el, note| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(note.to_string()),
            )
        })
        .child(div().flex_1())
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(group(n)),
        )
        .child(
            div()
                .w(px(64.))
                .text_right()
                .font_weight(FontWeight::MEDIUM)
                .child(format_tokens(n)),
        )
}

/// A thin horizontal meter.
fn bar(fraction: f32, color: Hsla, theme: &Theme) -> Div {
    div()
        .w_full()
        .h(px(4.))
        .rounded_full()
        .bg(theme.secondary)
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(color)
                .w(relative(fraction.clamp(0., 1.))),
        )
}

fn context_color(f: f32, theme: &Theme) -> Hsla {
    if f > 0.9 {
        theme.danger
    } else if f > 0.75 {
        theme.warning
    } else {
        theme.primary
    }
}

/// "$0.51", or "$0.0042" for small non-zero amounts, "Free" for zero.
fn money(cost: f64) -> String {
    if cost == 0. {
        "Free".into()
    } else if cost < 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

/// "29,434,622".
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{group, money};

    #[test]
    fn formats_money_and_counts() {
        assert_eq!(money(0.), "Free");
        assert_eq!(money(0.507687), "$0.51");
        assert_eq!(money(0.0042), "$0.0042");
        assert_eq!(group(29434622), "29,434,622");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
    }
}
