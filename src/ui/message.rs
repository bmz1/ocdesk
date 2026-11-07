//! Renders one message as plain text (markdown arrives in M2, tool cards in M3).

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use opencode_codes::protocol_generated::types as oc;

use crate::store::{MessageEntry, error_message};

/// Width of the conversation column, Codex-style.
pub const COLUMN_MAX_W: f32 = 760.;

pub fn render_message(entry: &MessageEntry, cx: &App) -> AnyElement {
    let theme = cx.theme();
    if entry.is_user() {
        let text: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                oc::Part::Text(t) if t.synthetic != Some(true) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.8))
                    .px_4()
                    .py_2p5()
                    .rounded_2xl()
                    .bg(theme.secondary)
                    .text_color(theme.secondary_foreground)
                    .child(text),
            )
            .into_any_element();
    }

    let mut col = v_flex().w_full().gap_3();
    for part in &entry.parts {
        col = match part {
            oc::Part::Text(t) if !t.text.is_empty() => {
                col.child(div().w_full().child(t.text.clone()))
            }
            oc::Part::Reasoning(r) if !r.text.trim().is_empty() => col.child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(div().font_weight(FontWeight::MEDIUM).child("Thinking"))
                    .child(
                        div()
                            .pl_3()
                            .border_l_2()
                            .border_color(theme.border)
                            .child(r.text.clone()),
                    ),
            ),
            oc::Part::Tool(t) => col.child(tool_row(t, cx)),
            _ => col,
        };
    }
    if let oc::Message::Assistant(a) = &entry.info
        && let Some(err) = &a.error
    {
        let msg = error_message(err);
        let aborted = serde_json::to_value(err)
            .ok()
            .and_then(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n == "MessageAbortedError")
            })
            .unwrap_or(false);
        col = col.child(
            h_flex()
                .gap_1p5()
                .text_sm()
                .text_color(if aborted {
                    theme.muted_foreground
                } else {
                    theme.danger
                })
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(if aborted { "Stopped".to_string() } else { msg }),
        );
    }
    col.into_any_element()
}

/// One quiet line per tool call, e.g. "▸ bash  echo hello · done".
fn tool_row(t: &oc::ToolPart, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let state = serde_json::to_value(&t.state).unwrap_or_default();
    let status = state
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("pending")
        .to_string();
    let input = state.get("input");
    let summary = state
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| {
            input
                .and_then(|i| i.get("command"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            input
                .and_then(|i| i.get("filePath"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            input
                .and_then(|i| i.get("description"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let status_color = match status.as_str() {
        "error" => theme.danger,
        _ => theme.muted_foreground,
    };
    h_flex()
        .gap_2()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(Icon::new(IconName::ChevronRight).xsmall())
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(t.tool.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .child(summary),
        )
        .child(div().text_color(status_color).child(status))
        .into_any_element()
}
