//! Renders one message: markdown prose (see `prose.rs`), reasoning rows,
//! work logs (see `tool_card.rs`), and the turn footer under a turn's last
//! reply.

use std::rc::Rc;

use super::tool_card::ToolCtx;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{AssistantMessage, Block, BlockKind, Message, Timing};
use crate::settings::Density;
use crate::ui::prose::{BlockKey, ProseCache, prose_blocks};

/// Width of the conversation column, Codex-style.
pub const COLUMN_MAX_W: f32 = 760.;

/// What a row's footer buttons ask the conversation to do.
#[derive(Clone, Debug, PartialEq)]
pub enum RowAction {
    /// Branch a new session holding the conversation up to (not including)
    /// `before`; `None` copies all of it.
    Fork { before: Option<String> },
    /// Roll the session (and its file changes) back to before this user
    /// message.
    Undo { message_id: String },
    /// Send this prompt again.
    Retry { text: String },
    /// Switch to the build agent and implement the plan above.
    Implement,
}

pub type OnRowAction = Rc<dyn Fn(RowAction, &mut Window, &mut App)>;

/// Where a row sits in its turn; computed by the conversation view.
pub struct RowCtx<'a> {
    /// The last assistant message of a finished turn.
    pub turn_end: bool,
    /// The assistant messages of this turn up to and including this row.
    pub turn: &'a [Message],
    /// The prompt that started the turn.
    pub prompt: Option<&'a crate::api::model::UserMessage>,
    /// The id of the message after this one, if any.
    pub next_id: Option<&'a str>,
    /// The session is idle: footers and Undo are offered.
    pub idle: bool,
    pub density: Density,
    pub on_action: OnRowAction,
}

/// Render row content. Every prose block of `msg` must have been synced into
/// `cache` first (`prose::sync_row`).
pub fn render_message(
    msg: &Message,
    cache: &Entity<ProseCache>,
    tools: &ToolCtx,
    row: &RowCtx,
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
            let group = SharedString::from(format!("user-{}", user.id));
            let undo = row.idle.then(|| {
                let on_action = row.on_action.clone();
                let message_id = user.id.clone();
                h_flex()
                    .gap_1()
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(
                        Clipboard::new(SharedString::from(format!("copy-prompt-{}", user.id)))
                            .value(user.text.clone())
                            .tooltip("Copy prompt"),
                    )
                    .child(
                        Button::new(SharedString::from(format!("undo-{}", user.id)))
                            .icon(IconName::Undo2)
                            .ghost()
                            .xsmall()
                            .tooltip("Undo from here: remove this prompt, its replies and their file changes")
                            .on_click(move |_, window, cx| {
                                on_action(
                                    RowAction::Undo {
                                        message_id: message_id.clone(),
                                    },
                                    window,
                                    cx,
                                )
                            }),
                    )
            });
            return v_flex()
                .w_full()
                .items_end()
                .gap_1()
                .group(group.clone())
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
                            let image = a
                                .path()
                                .and_then(|p| {
                                    p.extension().map(|s| s.to_string_lossy().to_lowercase())
                                })
                                .is_some_and(|e| {
                                    ["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp", "tiff"]
                                        .contains(&e.as_str())
                                });
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    Icon::new(if image {
                                        IconName::GalleryVerticalEnd
                                    } else {
                                        IconName::File
                                    })
                                    .xsmall(),
                                )
                                .child(a.name.clone())
                        })),
                )
                .children(undo)
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
    // Consecutive tool calls render together (folded into one row), and so
    // do consecutive reasoning blocks; prose that renders nothing doesn't
    // break either run.
    let mut run: Vec<&crate::api::model::ToolCall> = Vec::new();
    let mut thoughts: Vec<Thought> = Vec::new();
    let flush_thoughts = |col: Div, thoughts: &mut Vec<Thought>| -> Div {
        if thoughts.is_empty() {
            return col;
        }
        let el = reasoning(std::mem::take(thoughts), prose, cache, row.density, cx);
        col.children(el)
    };
    for block in &a.blocks {
        match block {
            Block::Prose {
                kind,
                text,
                done,
                time,
                ..
            } => {
                // `prose_blocks` yields one key per prose block, in order.
                let Some((key, _)) = keys.next() else {
                    continue;
                };
                match kind {
                    BlockKind::Text if !text.is_empty() => {
                        if let Some(s) = prose.state(&key) {
                            col = col
                                .children(super::tool_card::render_tools(&run, &a.id, tools, cx));
                            run.clear();
                            col = flush_thoughts(col, &mut thoughts);
                            col = col.child(markdown(&key, s));
                        }
                    }
                    BlockKind::Reasoning if !text.trim().is_empty() => {
                        col = col.children(super::tool_card::render_tools(&run, &a.id, tools, cx));
                        run.clear();
                        thoughts.push(Thought {
                            key,
                            done: *done,
                            time: *time,
                            last_line: last_line(text),
                        });
                    }
                    _ => {}
                }
            }
            Block::Tool(t) => {
                col = flush_thoughts(col, &mut thoughts);
                run.push(t);
            }
        }
    }
    col = flush_thoughts(col, &mut thoughts);
    col = col.children(super::tool_card::render_tools(&run, &a.id, tools, cx));
    if let Some(err) = &a.error {
        col = col.child(error_card(a, err, row, cx));
    }
    if row.turn_end && a.meta.agent.as_deref() == Some("plan") && a.error.is_none() {
        col = col.child(plan_bar(a, row, cx));
    }
    if row.turn_end {
        col = col.child(turn_footer(a, row, cx));
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

/// One reasoning block of a run of them.
struct Thought {
    key: BlockKey,
    done: bool,
    time: Timing,
    last_line: String,
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_matches(|c| c == '*' || c == '#' || c == '_')
        .to_string()
}

/// "Thought for 4s" / "Thinking… <latest line>". Adjacent reasoning
/// blocks share one row; collapsed by default (expanded when verbose,
/// hidden once done when quiet).
fn reasoning(
    thoughts: Vec<Thought>,
    prose: &ProseCache,
    cache: &Entity<ProseCache>,
    density: Density,
    cx: &App,
) -> Option<AnyElement> {
    let theme = cx.theme();
    let done = thoughts.iter().all(|t| t.done);
    if done && density == Density::Quiet {
        return None;
    }
    let key = thoughts[0].key.clone();
    let open = prose.is_expanded(&key) != (density == Density::Verbose);
    let label = if done {
        thought_label(thoughts.iter().map(|t| &t.time))
    } else {
        "Thinking…".to_string()
    };
    let preview = (!done && !open)
        .then(|| thoughts.last().map(|t| t.last_line.clone()))
        .flatten()
        .filter(|l| !l.is_empty());
    let bodies: Vec<AnyElement> = if open {
        thoughts
            .iter()
            .filter_map(|t| prose.state(&t.key).map(|s| markdown(&t.key, s)))
            .map(|m| m.into_any_element())
            .collect()
    } else {
        Vec::new()
    };
    let cache = cache.clone();
    let toggle_key = key.clone();
    Some(
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .id(key.element_id("reasoning-toggle"))
                    .gap_1p5()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.foreground))
                    .child(super::icons::brain().xsmall())
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(label),
                    )
                    .when_some(preview, |el, p| {
                        el.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .italic()
                                .text_color(theme.muted_foreground.opacity(0.8))
                                .child(p),
                        )
                    })
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
            .when(!bodies.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .border_color(theme.border)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .children(bodies),
                )
            })
            .into_any_element(),
    )
}

/// "Thought for 4s" from the blocks' times; plain "Thought" without them.
pub fn thought_label<'a>(times: impl Iterator<Item = &'a Timing>) -> String {
    let mut total = 0u64;
    let mut known = false;
    for t in times {
        if let (Some(a), Some(b)) = (t.created, t.completed) {
            total += b.saturating_sub(a);
            known = true;
        }
    }
    if !known {
        return "Thought".into();
    }
    format!("Thought for {}", format_duration_ms(total))
}

/// "<1s", "4s", "2m 5s", "1h 3m".
pub fn format_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => "<1s".into(),
        1..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// "12.3K" style token counts.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// The plain text of a turn's replies, for Copy.
fn turn_text(turn: &[Message]) -> String {
    turn.iter()
        .filter_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        })
        .flat_map(|a| a.blocks.iter())
        .filter_map(|b| match b {
            Block::Prose {
                kind: BlockKind::Text,
                text,
                ..
            } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Under a finished turn, shown on hover: Copy · duration · model · tokens
/// · Fork. Usage sums the turn's steps.
fn turn_footer(a: &AssistantMessage, row: &RowCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let group = SharedString::from(format!("turn-{}", a.id));
    let mut facts: Vec<String> = Vec::new();
    let started = row.prompt.and_then(|p| p.meta.created).or_else(|| {
        row.turn.iter().find_map(|m| match m {
            Message::Assistant(a) => a.meta.created,
            _ => None,
        })
    });
    if let (Some(s), Some(e)) = (started, a.meta.completed) {
        facts.push(format_duration_ms(e.saturating_sub(s)));
    }
    if let Some(m) = &a.meta.model {
        facts.push(match &m.variant {
            Some(v) => format!("{} · {v}", m.id),
            None => m.id.clone(),
        });
    }
    let mut tokens = 0u64;
    let mut cost = 0f64;
    for m in row.turn {
        if let Message::Assistant(x) = m {
            if let Some(t) = &x.meta.tokens {
                tokens += t.input + t.output + t.reasoning;
            }
            cost += x.meta.cost.unwrap_or(0.);
        }
    }
    if tokens > 0 {
        facts.push(format!("{} tokens", format_tokens(tokens)));
    }
    if cost > 0. {
        facts.push(format!("${cost:.2}"));
    }
    let on_action = row.on_action.clone();
    let before = row.next_id.map(str::to_string);
    h_flex()
        .id(group.clone())
        .group(group.clone())
        .w_full()
        .min_h(px(24.))
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(
            Clipboard::new(SharedString::from(format!("copy-turn-{}", a.id)))
                .value(turn_text(row.turn))
                .tooltip("Copy reply"),
        )
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .child(div().child(facts.join(" · ")))
                .child(
                    Button::new(SharedString::from(format!("fork-{}", a.id)))
                        .icon(super::icons::git_fork())
                        .ghost()
                        .xsmall()
                        .tooltip("Fork from here: a new session with the conversation so far")
                        .on_click(move |_, window, cx| {
                            on_action(
                                RowAction::Fork {
                                    before: before.clone(),
                                },
                                window,
                                cx,
                            )
                        }),
                ),
        )
        .into_any_element()
}

/// A failed or stopped step: a tinted card with Retry and Copy.
fn error_card(a: &AssistantMessage, err: &str, row: &RowCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let stopped = err == "Step interrupted";
    if stopped {
        return h_flex()
            .gap_1p5()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(super::icons::circle_stop().xsmall())
            .child("Stopped")
            .into_any_element();
    }
    let retry = row.prompt.filter(|_| row.turn_end && row.idle).map(|p| {
        let on_action = row.on_action.clone();
        let text = p.text.clone();
        Button::new(SharedString::from(format!("retry-{}", a.id)))
            .label("Retry")
            .icon(IconName::RefreshCw)
            .xsmall()
            .outline()
            .on_click(move |_, window, cx| {
                on_action(RowAction::Retry { text: text.clone() }, window, cx)
            })
    });
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.danger.opacity(0.5))
        .bg(theme.danger.opacity(0.08))
        .text_sm()
        .child(
            div()
                .pt(px(2.))
                .text_color(theme.danger)
                .child(Icon::new(IconName::CircleAlert).xsmall()),
        )
        .child(div().flex_1().min_w_0().child(err.to_string()))
        .child(
            Clipboard::new(SharedString::from(format!("copy-error-{}", a.id)))
                .value(err.to_string())
                .tooltip("Copy error"),
        )
        .children(retry)
        .into_any_element()
}

/// Under a finished plan-agent turn: implement it with the build agent.
fn plan_bar(a: &AssistantMessage, row: &RowCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let on_action = row.on_action.clone();
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.35))
        .text_sm()
        .child(super::icons::list_checks().small())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child("Plan ready. Implement it with the build agent?"),
        )
        .child(
            Button::new(SharedString::from(format!("implement-{}", a.id)))
                .label("Implement plan")
                .small()
                .primary()
                .disabled(!row.idle)
                .on_click(move |_, window, cx| on_action(RowAction::Implement, window, cx)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn durations_and_tokens_format_compactly() {
        assert_eq!(format_duration_ms(400), "<1s");
        assert_eq!(format_duration_ms(4_200), "4s");
        assert_eq!(format_duration_ms(125_000), "2m 5s");
        assert_eq!(format_duration_ms(3_780_000), "1h 3m");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(64_600), "64.6K");
    }

    #[::core::prelude::v1::test]
    fn thought_label_sums_known_times() {
        let t = |a, b| Timing {
            created: Some(a),
            completed: Some(b),
        };
        assert_eq!(
            thought_label([t(0, 1500), t(2000, 5000)].iter()),
            "Thought for 4s"
        );
        assert_eq!(thought_label([Timing::default()].iter()), "Thought");
    }

    #[::core::prelude::v1::test]
    fn last_line_skips_blank_and_markdown_marks() {
        assert_eq!(
            last_line("first\n**Checking the tests**\n\n"),
            "Checking the tests"
        );
    }
}
