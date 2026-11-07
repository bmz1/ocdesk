//! Tool details stay collapsed until requested; large output is bounded on screen.
use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{ToolCall, ToolStatus};

pub type ToggleTool = Rc<dyn Fn(String, &mut App)>;

pub fn key(message_id: &str, call_id: &str) -> String {
    format!("{message_id}/{call_id}")
}

pub fn render_tool(
    t: &ToolCall,
    message_id: &str,
    expanded: &HashSet<String>,
    toggle: ToggleTool,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let key = key(message_id, &t.call_id);
    let open = expanded.contains(&key);
    let summary = [
        "description",
        "command",
        "filePath",
        "path",
        "pattern",
        "url",
    ]
    .iter()
    .find_map(|k| t.input.get(*k).and_then(|v| v.as_str()))
    .map(|s| s.lines().next().unwrap_or("").to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| t.name.clone());
    // Codex-style verdict verb; a failure is the one loud state.
    let (verb, color) = match t.status {
        ToolStatus::Pending => ("Queued", theme.muted_foreground),
        ToolStatus::Running => ("Running", theme.warning),
        ToolStatus::Completed => ("Ran", theme.muted_foreground),
        ToolStatus::Error => ("Failed to run", theme.danger),
    };
    let click_key = key.clone();
    let header = h_flex()
        .id(SharedString::from(format!("tool-{key}")))
        .w_full()
        .gap_2()
        .cursor_pointer()
        .on_click(move |_, _, cx| toggle(click_key.clone(), cx))
        .child(
            div()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(">_"),
        )
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .child(verb),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(summary),
        )
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(if open { "⌄" } else { "›" }),
        );
    v_flex()
        .w_full()
        .text_sm()
        .child(header)
        .when(open, |col| {
            let input = serde_json::to_string_pretty(&t.input).unwrap_or_default();
            let mut details = v_flex().w_full().gap_2().pt_1();
            // Result first: output, or the error for failed tools.
            match (&t.error, t.output.is_empty()) {
                (Some(error), _) => {
                    details = details.child(result_panel(&format!("error-{key}"), error, cx));
                }
                (None, false) => {
                    details = details.child(result_panel(&format!("output-{key}"), &t.output, cx));
                }
                _ => {}
            }
            if let Some(exit) = t.metadata.get("exit") {
                details = details.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("Exit code: {exit}")),
                );
            }
            if t.metadata.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
                details = details.child(
                    div()
                        .text_color(theme.warning)
                        .child("Output truncated by opencode"),
                );
            }
            // Tool metadata may carry the edit's unified diff before the turn ends.
            if let Some(diff) = t.metadata.get("diff").and_then(|v| v.as_str()) {
                details = details.child(super::changes::patch_view(
                    &format!("tool-diff-{key}"),
                    diff,
                    cx,
                ));
            }
            details = details.child(text_panel(&format!("input-{key}"), "Input", &input, cx));
            col.child(details)
        })
        .into_any_element()
}

/// The rounded, darker mono panel that carries a tool's result (or error).
pub fn result_panel(id: &str, text: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let copy = text.to_string();
    // Keep the full output for copying while bounding GPUI's text layout work.
    let preview: String = text.chars().take(24_000).collect();
    let clipped = preview.len() < text.len();
    v_flex()
        .w_full()
        .gap_1()
        .rounded_md()
        .bg(theme.secondary)
        .p_2()
        .child(
            h_flex()
                .justify_between()
                .items_start()
                .child(
                    div()
                        .id(SharedString::from(id.to_string()))
                        .max_h(px(260.))
                        .overflow_y_scroll()
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_font_family.clone())
                        .child(preview),
                )
                .child(
                    Button::new(SharedString::from(format!("copy-{id}")))
                        .label("Copy")
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                ),
        )
        .when(clipped, |el| {
            el.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Preview shortened. Copy includes the full text."),
            )
        })
        .into_any_element()
}

pub fn text_panel(id: &str, title: &str, text: &str, cx: &App) -> AnyElement {
    let copy = text.to_string();
    // Keep the full output for copying while bounding GPUI's text layout work.
    let preview: String = text.chars().take(24_000).collect();
    let clipped = preview.len() < text.len();
    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(title.to_string()),
                )
                .child(
                    Button::new(SharedString::from(format!("copy-{id}")))
                        .label("Copy")
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                ),
        )
        .child(
            div()
                .id(SharedString::from(id.to_string()))
                .max_h(px(260.))
                .overflow_y_scroll()
                .font_family(cx.theme().mono_font_family.clone())
                .child(preview),
        )
        .when(clipped, |el| {
            el.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("Preview shortened. Copy includes the full text."),
            )
        })
        .into_any_element()
}
