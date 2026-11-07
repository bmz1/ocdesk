//! Renders one message: markdown prose (see `prose.rs`), collapsible
//! reasoning, and a quiet row per tool call (tool cards arrive in M3).

use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::model::{Block, BlockKind, Message, ToolCall, ToolStatus};
use crate::ui::prose::{BlockKey, ProseCache, prose_blocks};

/// Width of the conversation column, Codex-style.
pub const COLUMN_MAX_W: f32 = 760.;

/// Render row content. Every prose block of `msg` must have been synced into
/// `cache` first (`prose::sync_row`).
pub fn render_message(msg: &Message, cache: &Entity<ProseCache>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let prose = cache.read(cx);
    let mut keys = prose_blocks(msg);

    let a = match msg {
        Message::User(_) => {
            let body = keys
                .next()
                .and_then(|(k, _)| prose.state(&k).map(|s| markdown(&k, s)));
            return h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.8))
                        .min_w_0()
                        .px_4()
                        .py_2p5()
                        .rounded_2xl()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .children(body),
                )
                .into_any_element();
        }
        Message::Assistant(a) => a,
    };

    let mut col = v_flex().w_full().gap_3();
    for block in &a.blocks {
        col = match block {
            Block::Prose {
                kind, text, done, ..
            } => {
                // `prose_blocks` yields one key per prose block, in order.
                let Some((key, _)) = keys.next() else {
                    continue;
                };
                match kind {
                    BlockKind::Text if !text.is_empty() => {
                        col.children(prose.state(&key).map(|s| markdown(&key, s)))
                    }
                    BlockKind::Reasoning if !text.trim().is_empty() => {
                        col.child(reasoning(key, *done, prose, cache, cx))
                    }
                    _ => col,
                }
            }
            Block::Tool(t) => col.child(tool_row(t, cx)),
        };
    }
    if let Some(err) = &a.error {
        let stopped = err == "Step interrupted";
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

fn markdown(key: &BlockKey, state: &Entity<TextViewState>) -> TextView {
    let block_id = key.element_id("code-copy");
    TextView::new(state)
        .w_full()
        .selectable(true)
        .code_block_actions(move |block, _, _| {
            let at = block.span.map_or(0, |s| s.start) as u64;
            let id = match &block_id {
                ElementId::NamedInteger(n, h) => ElementId::NamedInteger(n.clone(), h ^ at),
                other => other.clone(),
            };
            Clipboard::new(id).value(block.code()).tooltip("Copy")
        })
}

/// Codex-style "Thinking" row, collapsed by default; click to expand.
fn reasoning(
    key: BlockKey,
    done: bool,
    prose: &ProseCache,
    cache: &Entity<ProseCache>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let open = prose.is_expanded(&key);
    let body = open.then(|| prose.state(&key)).flatten().map(|s| {
        div()
            .pl_3()
            .border_l_2()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(markdown(&key, s))
    });
    let cache = cache.clone();
    let toggle_key = key.clone();
    v_flex()
        .gap_1p5()
        .child(
            h_flex()
                .id(key.element_id("reasoning-toggle"))
                .gap_1()
                .text_sm()
                .text_color(theme.muted_foreground)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.foreground))
                .child(div().font_weight(FontWeight::MEDIUM).child(if done {
                    "Thought"
                } else {
                    "Thinking…"
                }))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall(),
                )
                .on_click(move |_, _, cx| {
                    let k = toggle_key.clone();
                    cache.update(cx, |c, cx| c.toggle_expanded(k, cx));
                }),
        )
        .children(body)
        .into_any_element()
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
