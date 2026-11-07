//! Renders one message: markdown prose (see `prose.rs`), collapsible
//! reasoning, and expandable tool cards.

use super::tool_card::ToolCtx;
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::model::{Block, BlockKind, Message};
use crate::ui::prose::{BlockKey, ProseCache, prose_blocks};

/// Width of the conversation column, Codex-style.
pub const COLUMN_MAX_W: f32 = 760.;

/// Render row content. Every prose block of `msg` must have been synced into
/// `cache` first (`prose::sync_row`).
pub fn render_message(
    msg: &Message,
    cache: &Entity<ProseCache>,
    tools: &ToolCtx,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let prose = cache.read(cx);
    let mut keys = prose_blocks(msg);

    let a = match msg {
        Message::User(user) => {
            let body = keys
                .next()
                .and_then(|(k, _)| prose.state(&k).map(|s| markdown(&k, s)));
            return h_flex()
                .w_full()
                .justify_end()
                .child(
                    v_flex()
                        .gap_1()
                        .max_w(relative(0.8))
                        .min_w_0()
                        .px_4()
                        .py_2p5()
                        .rounded_2xl()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .children(body)
                        .children(user.attachments.iter().map(|a| {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{} {}",
                                    if a.path()
                                        .and_then(|p| p
                                            .extension()
                                            .map(|s| s.to_string_lossy().to_lowercase()))
                                        .is_some_and(|e| [
                                            "png", "jpg", "jpeg", "webp", "gif", "svg", "bmp",
                                            "tiff"
                                        ]
                                        .contains(&e.as_str()))
                                    {
                                        "Image:"
                                    } else {
                                        "File:"
                                    },
                                    a.name
                                ))
                        })),
                )
                .into_any_element();
        }
        Message::Assistant(a) => a,
        Message::Marker(m) => {
            // A quiet divider: ─── Context compacted ───
            let color = if m.failed {
                theme.danger
            } else {
                theme.muted_foreground
            };
            let rule = || div().flex_1().h(px(1.)).bg(theme.border);
            return h_flex()
                .w_full()
                .gap_3()
                .items_center()
                .text_xs()
                .text_color(color)
                .child(rule())
                .child(
                    div()
                        .max_w(relative(0.8))
                        .text_center()
                        .child(m.text.clone()),
                )
                .child(rule())
                .into_any_element();
        }
    };

    let mut col = v_flex().w_full().gap_3();
    // Consecutive tool calls render together (folded into one row); prose
    // that renders nothing doesn't break the run.
    let mut run: Vec<&crate::api::model::ToolCall> = Vec::new();
    for block in &a.blocks {
        match block {
            Block::Prose {
                kind, text, done, ..
            } => {
                // `prose_blocks` yields one key per prose block, in order.
                let Some((key, _)) = keys.next() else {
                    continue;
                };
                let el = match kind {
                    BlockKind::Text if !text.is_empty() => prose
                        .state(&key)
                        .map(|s| markdown(&key, s).into_any_element()),
                    BlockKind::Reasoning if !text.trim().is_empty() => {
                        Some(reasoning(key, *done, prose, cache, cx))
                    }
                    _ => None,
                };
                if let Some(el) = el {
                    col = col.children(super::tool_card::render_tools(&run, &a.id, tools, cx));
                    run.clear();
                    col = col.child(el);
                }
            }
            Block::Tool(t) => run.push(t),
        }
    }
    col = col.children(super::tool_card::render_tools(&run, &a.id, tools, cx));
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
