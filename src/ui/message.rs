//! Renders one message as plain text (markdown arrives in M2, tool cards in M3).

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::model::{Block, BlockKind, Message, ToolCall, ToolStatus};

/// Width of the conversation column, Codex-style.
pub const COLUMN_MAX_W: f32 = 760.;

pub fn render_message(msg: &Message, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let a = match msg {
        Message::User(u) => {
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
                        .child(u.text.clone()),
                )
                .into_any_element();
        }
        Message::Assistant(a) => a,
    };

    let mut col = v_flex().w_full().gap_3();
    for block in &a.blocks {
        col = match block {
            Block::Prose {
                kind: BlockKind::Text,
                text,
                ..
            } if !text.is_empty() => col.child(div().w_full().child(text.clone())),
            Block::Prose {
                kind: BlockKind::Reasoning,
                text,
                ..
            } if !text.trim().is_empty() => col.child(
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
                            .child(text.clone()),
                    ),
            ),
            Block::Tool(t) => col.child(tool_row(t, cx)),
            _ => col,
        };
    }
    if let Some(err) = &a.error {
        let stopped = err == "Provider turn interrupted";
        col = col.child(
            h_flex()
                .gap_1p5()
                .text_sm()
                .text_color(if stopped {
                    theme.muted_foreground
                } else {
                    theme.danger
                })
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(if stopped {
                    "Stopped".to_string()
                } else {
                    err.clone()
                }),
        );
    }
    col.into_any_element()
}

/// One quiet line per tool call, e.g. "› bash  echo hello   done".
fn tool_row(t: &ToolCall, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let input = &t.input;
    let summary = [
        "command",
        "filePath",
        "path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
    .unwrap_or("")
    .lines()
    .next()
    .unwrap_or("")
    .to_string();
    let (status, color) = match t.status {
        ToolStatus::Pending => ("pending", theme.muted_foreground),
        ToolStatus::Running => ("running", theme.muted_foreground),
        ToolStatus::Completed => ("done", theme.muted_foreground),
        ToolStatus::Error => ("error", theme.danger),
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
                .child(t.name.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .child(summary),
        )
        .child(div().text_color(color).child(status))
        .into_any_element()
}
