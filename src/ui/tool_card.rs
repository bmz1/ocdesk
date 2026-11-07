//! Tool calls, Codex-style: a quiet "Ran git status…" row that expands into
//! a card (tool name, the `$ command` with a copy button, then its output).
//! Consecutive calls fold into one "Ran 2 commands, read 1 file" row.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{ToolCall, ToolStatus};

pub type ToggleTool = Rc<dyn Fn(String, &mut App)>;

/// Scroll positions of expanded output panels, so they survive re-renders
/// and can hand wheel events to the message list at their edges.
pub type ScrollHandles = Rc<RefCell<HashMap<String, ScrollHandle>>>;

/// What tool rows need from the conversation view.
pub struct ToolCtx<'a> {
    pub expanded: &'a HashSet<String>,
    pub toggle: ToggleTool,
    pub open_task: &'a super::tasks::OpenTask,
    pub scrolls: &'a ScrollHandles,
    pub density: crate::settings::Density,
    /// The find bar's query, lowercased: calls that match open up.
    pub find: Option<&'a str>,
}

impl ToolCtx<'_> {
    /// Whether the row or group under `key` is open. In the verbose
    /// density rows start open and a click closes them.
    pub fn is_open(&self, key: &str) -> bool {
        let verbose = self.density == crate::settings::Density::Verbose;
        self.expanded.contains(key) != verbose
    }

    fn found(&self, t: &ToolCall) -> bool {
        self.find.is_some_and(|q| tool_matches(t, q))
    }
}

/// Whether a call's input, output or error contains `q` (lowercase).
pub fn tool_matches(t: &ToolCall, q: &str) -> bool {
    let has = |s: &str| s.to_lowercase().contains(q);
    has(&t.name)
        || has(&t.input.to_string())
        || has(&t.output)
        || t.error.as_deref().is_some_and(has)
}

/// Successful, finished calls are left out in the quiet density.
fn hidden_in(t: &ToolCall, density: crate::settings::Density) -> bool {
    density == crate::settings::Density::Quiet && t.status == ToolStatus::Completed
}

/// Output panels taller than this scroll.
const OUTPUT_MAX_H: f32 = 240.;
/// Bound GPUI's text layout work; Copy still gets the full text.
const PREVIEW_CHARS: usize = 24_000;

/// Subagent calls and backgrounded shells render as one quiet row that
/// opens the background tasks drawer, instead of a tool card.
fn task_row(
    t: &ToolCall,
    message_id: &str,
    open_task: &super::tasks::OpenTask,
    cx: &App,
) -> Option<AnyElement> {
    use super::tasks::{TaskView, background_shell, subagent_session};
    let theme = cx.theme();
    let (icon, title, detail, target) = if t.name == "subagent" {
        let title = str_input(t, "description")
            .unwrap_or("Subagent")
            .to_string();
        let agent = str_input(t, "agent").unwrap_or("subagent");
        let state = match t.status {
            ToolStatus::Pending | ToolStatus::Running => "running",
            ToolStatus::Completed => "done",
            ToolStatus::Error => "failed",
        };
        (
            super::icons::workflow(),
            title,
            format!("@{agent} · {state}"),
            subagent_session(t).map(TaskView::Subagent),
        )
    } else {
        let shell = background_shell(t)?;
        // Only calls the server moved to the background.
        if !t.output.contains("moved to the background") {
            return None;
        }
        let cmd = first_line(str_input(t, "command").unwrap_or("command"));
        (
            super::icons::terminal(),
            cmd,
            "running in the background".to_string(),
            Some(TaskView::Shell(shell)),
        )
    };
    let running = is_running(t);
    let failed = t.status == ToolStatus::Error;
    let open = open_task.clone();
    Some(
        h_flex()
            .id(SharedString::from(format!(
                "task-row-{message_id}-{}",
                t.call_id
            )))
            .gap_2()
            .text_sm()
            .text_color(if failed {
                theme.danger
            } else {
                theme.muted_foreground
            })
            .when(target.is_some(), |el| {
                el.cursor_pointer()
                    .hover(|s| s.text_color(theme.foreground))
            })
            .child(icon.small())
            .child(div().min_w_0().truncate().child(title))
            .child(div().flex_none().text_xs().child(detail))
            .when(running, |el| {
                el.child(Spinner::new().xsmall().color(theme.muted_foreground))
            })
            .when_some(target, |el, target| {
                el.child(Icon::new(IconName::ChevronRight).xsmall())
                    .on_click(move |_, _, cx| open(target.clone(), cx))
            })
            .into_any_element(),
    )
}

/// A `todowrite` call: "Updated todos · 2/5 done", opening into the list
/// it set.
fn todo_row(t: &ToolCall, message_id: &str, ctx: &ToolCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let todos = super::todos::of_call(t);
    let (done, total) = super::todos::progress(&todos);
    let key = key(message_id, &t.call_id);
    let open = ctx.is_open(&key);
    let toggle = ctx.toggle.clone();
    let click_key = key.clone();
    let label = match (t.status, total) {
        (ToolStatus::Error, _) => "Couldn't update the todo list".to_string(),
        (_, 0) => "Cleared the todo list".to_string(),
        _ => format!("Updated todos · {done}/{total} done"),
    };
    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .id(SharedString::from(format!("todo-{key}")))
                .gap_1p5()
                .items_center()
                .text_sm()
                .text_color(if t.status == ToolStatus::Error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .cursor_pointer()
                .hover(|s| s.text_color(theme.foreground))
                .on_click(move |_, _, cx| toggle(click_key.clone(), cx))
                .child(super::icons::list_checks().xsmall())
                .child(label)
                .when(total > 0, |el| el.child(super::todos::bar(done, total, cx)))
                .when(total > 0, |el| el.child(chevron(open))),
        )
        .when(open && total > 0, |el| {
            el.child(
                div()
                    .pl_3()
                    .border_l_1()
                    .border_color(theme.border)
                    .child(super::todos::checklist(&todos, cx)),
            )
        })
        .into_any_element()
}

pub fn key(message_id: &str, call_id: &str) -> String {
    format!("{message_id}/{call_id}")
}

/// A run of consecutive tool calls from one message. Task rows (subagents,
/// background shells) stand alone; two or more plain calls fold into one
/// summary row that expands into the individual rows.
pub fn render_tools(
    tools: &[&ToolCall],
    message_id: &str,
    ctx: &ToolCtx,
    cx: &App,
) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let mut run: Vec<&ToolCall> = Vec::new();
    let flush = |run: &mut Vec<&ToolCall>, out: &mut Vec<AnyElement>| {
        match run.len() {
            0 => {}
            1 => out.push(render_tool(run[0], message_id, ctx, cx)),
            _ => out.push(render_group(run, message_id, ctx, cx)),
        }
        run.clear();
    };
    for t in tools {
        if hidden_in(t, ctx.density) && t.name != "subagent" {
            continue;
        }
        if t.name == "todowrite" {
            flush(&mut run, &mut out);
            out.push(todo_row(t, message_id, ctx, cx));
            continue;
        }
        if let Some(row) = task_row(t, message_id, ctx.open_task, cx) {
            flush(&mut run, &mut out);
            out.push(row);
        } else {
            run.push(t);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn render_group(tools: &[&ToolCall], message_id: &str, ctx: &ToolCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let key = key(message_id, &format!("group-{}", tools[0].call_id));
    let open = ctx.is_open(&key) || tools.iter().any(|t| ctx.found(t));
    let current = tools.iter().rev().find(|t| is_running(t));
    let running = current.is_some();
    let failed = tools
        .iter()
        .filter(|t| t.status == ToolStatus::Error)
        .count();
    // While a call runs, the row names it ("Running cargo test"); once the
    // run settles it becomes the summary.
    let summary = match current {
        Some(t) => format!("{} {}", verb(t), row_summary(t)),
        None => tools_summary(tools.iter().copied()).unwrap_or_default(),
    };
    let toggle = ctx.toggle.clone();
    let click_key = key.clone();
    let header = h_flex()
        .id(SharedString::from(format!("tools-{key}")))
        .max_w_full()
        .gap_1p5()
        .text_sm()
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|s| s.text_color(theme.foreground))
        .on_click(move |_, _, cx| toggle(click_key.clone(), cx))
        .child(div().min_w_0().truncate().child(summary))
        .when(failed > 0, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_color(theme.danger)
                    .child(format!("· {failed} failed")),
            )
        })
        .when(running, |el| {
            el.child(Spinner::new().xsmall().color(theme.muted_foreground))
        })
        .child(chevron(open));
    v_flex()
        .w_full()
        .gap_2()
        .child(header)
        .when(open, |col| {
            col.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .pl_3()
                    .border_l_1()
                    .border_color(theme.border)
                    .children(tools.iter().map(|t| render_tool(t, message_id, ctx, cx))),
            )
        })
        .into_any_element()
}

fn render_tool(t: &ToolCall, message_id: &str, ctx: &ToolCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let key = key(message_id, &t.call_id);
    let open = ctx.is_open(&key) || ctx.found(t);
    let verb_color = if t.status == ToolStatus::Error {
        theme.danger
    } else {
        theme.muted_foreground
    };
    let toggle = ctx.toggle.clone();
    let click_key = key.clone();
    let header = h_flex()
        .id(SharedString::from(format!("tool-{key}")))
        .max_w_full()
        .gap_1p5()
        .text_sm()
        .cursor_pointer()
        .text_color(theme.muted_foreground)
        .hover(|s| s.text_color(theme.foreground))
        .on_click(move |_, _, cx| toggle(click_key.clone(), cx))
        .child(div().flex_none().text_color(verb_color).child(verb(t)))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.foreground)
                .child(row_summary(t)),
        )
        .when(is_running(t), |el| {
            el.child(Spinner::new().xsmall().color(theme.muted_foreground))
        })
        .child(chevron(open));
    v_flex()
        .w_full()
        .gap_2()
        .child(header)
        .when(open, |col| col.child(tool_details(t, &key, ctx, cx)))
        .into_any_element()
}

/// The expanded card: tool name, its input (a `$ command` for shells),
/// then the result.
fn tool_details(t: &ToolCall, key: &str, ctx: &ToolCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let shell = is_shell(t);
    let raw_key = format!("raw-{key}");
    let show_raw = ctx.expanded.contains(&raw_key);
    // Tools without a main argument we know show their JSON input only on
    // request; the summary row already names what they did.
    let main = main_argument(t);
    let input = if show_raw {
        serde_json::to_string_pretty(&t.input).unwrap_or_default()
    } else {
        main.clone().unwrap_or_else(|| input_text(t))
    };
    let has_raw = main.is_some() && !shell && t.input.as_object().is_some_and(|o| o.len() > 1);
    let input_block = h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .when(shell, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child("$"),
            )
        })
        .child(div().flex_1().min_w_0().child(if shell {
            shell_highlight(&input, cx).into_any_element()
        } else {
            div()
                .text_color(theme.foreground)
                .child(input.clone())
                .into_any_element()
        }))
        .child(
            Clipboard::new(SharedString::from(format!("copy-input-{key}")))
                .value(input)
                .tooltip("Copy"),
        );

    let mut card = v_flex()
        .w_full()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.35))
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(display_name(&t.name))
                .when(has_raw, |el| {
                    let toggle = ctx.toggle.clone();
                    el.child(
                        div()
                            .id(SharedString::from(format!("raw-toggle-{key}")))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .child(if show_raw {
                                "Hide raw input"
                            } else {
                                "Raw input"
                            })
                            .on_click(move |_, _, cx| toggle(raw_key.clone(), cx)),
                    )
                }),
        )
        .child(input_block);

    // The file a call is about: its output (reads) and content (writes)
    // are coloured as that file's language.
    let file = str_input(t, "filePath").or(str_input(t, "path"));
    if t.name == "write"
        && let Some(content) = str_input(t, "content").filter(|c| !c.trim().is_empty())
    {
        card = card.child(output_panel(
            &format!("content-{key}"),
            content.trim_end(),
            theme.foreground,
            file,
            ctx,
            cx,
        ));
    }
    // Result: the error for failed tools, else the output.
    match (&t.error, t.output.trim_end()) {
        (Some(error), _) => {
            card = card.child(output_panel(
                &format!("error-{key}"),
                error,
                theme.danger,
                None,
                ctx,
                cx,
            ));
        }
        (None, out) if !out.is_empty() && t.name != "write" => {
            let read = kind(&t.name) == Kind::Read;
            card = card.child(output_panel(
                &format!("output-{key}"),
                out,
                if read {
                    theme.foreground
                } else {
                    theme.muted_foreground
                },
                file.filter(|_| read),
                ctx,
                cx,
            ));
        }
        _ if is_running(t) => {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Running…"),
            );
        }
        _ => {}
    }
    let mut notes = Vec::new();
    if let Some(exit) = t.metadata.get("exit").filter(|v| v.as_i64() != Some(0)) {
        notes.push(format!("Exit code {exit}"));
    }
    if t.metadata.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
        notes.push("Output truncated by opencode".into());
    }
    if !notes.is_empty() {
        card = card.child(
            div()
                .text_xs()
                .text_color(theme.warning)
                .child(notes.join(" · ")),
        );
    }
    // Tool metadata may carry the edit's unified diff before the turn ends.
    if let Some(diff) = t.metadata.get("diff").and_then(|v| v.as_str()) {
        card = card.child(super::changes::patch_view(
            &format!("tool-diff-{key}"),
            diff,
            file,
            cx,
        ));
    }
    card.into_any_element()
}

/// Mono output, bounded in height. Scrolling inside it doesn't drag the
/// conversation along; at either end the wheel goes back to the list.
fn output_panel(
    id: &str,
    text: &str,
    color: Hsla,
    language: Option<&str>,
    ctx: &ToolCtx,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let preview: String = text.chars().take(PREVIEW_CHARS).collect();
    let clipped = preview.len() < text.len();
    let body: AnyElement = match language {
        Some(lang) if super::syntax::supported(lang) => {
            let spans = super::syntax::highlight_gutter(lang, &preview);
            super::syntax::styled(preview, &spans, cx).into_any_element()
        }
        _ => preview.into_any_element(),
    };
    let handle = ctx
        .scrolls
        .borrow_mut()
        .entry(id.to_string())
        .or_default()
        .clone();
    v_flex()
        .w_full()
        .gap_1()
        .child(
            chained_scroll(id, &handle)
                .max_h(px(OUTPUT_MAX_H))
                .w_full()
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .text_color(color)
                .child(body),
        )
        .when(clipped, |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Preview shortened."),
            )
        })
        .into_any_element()
}

/// A vertical scroll area nested in the message list. GPUI lets both scroll
/// on the same wheel event; this keeps the event while the area can still
/// move and lets it through at the ends.
fn chained_scroll(id: &str, handle: &ScrollHandle) -> Stateful<Div> {
    let h = handle.clone();
    div()
        .id(SharedString::from(id.to_string()))
        .overflow_y_scroll()
        .track_scroll(handle)
        // Runs after the area's own scroll listener applied the delta, and
        // before layout clamps it: out of range means it hit an end.
        .on_scroll(move |_, _, cx| {
            let y = h.offset().y;
            let max = h.max_offset().y;
            if max > px(0.) && y <= px(0.) && y >= -max {
                cx.stop_propagation();
            }
        })
}

fn chevron(open: bool) -> Icon {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .xsmall()
}

fn str_input<'a>(t: &'a ToolCall, k: &str) -> Option<&'a str> {
    t.input.get(k).and_then(|v| v.as_str())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn is_running(t: &ToolCall) -> bool {
    matches!(t.status, ToolStatus::Pending | ToolStatus::Running)
}

fn is_shell(t: &ToolCall) -> bool {
    matches!(t.name.as_str(), "shell" | "bash" | "execute") && str_input(t, "command").is_some()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Ran,
    Read,
    Edited,
    Searched,
    Other,
}

fn kind(name: &str) -> Kind {
    match name {
        "shell" | "bash" | "execute" => Kind::Ran,
        "read" => Kind::Read,
        "edit" | "write" | "patch" | "multiedit" => Kind::Edited,
        "grep" | "glob" | "list" | "websearch" | "webfetch" | "codesearch" => Kind::Searched,
        _ => Kind::Other,
    }
}

/// "Ran" / "Running", "Read" / "Reading", …
fn verb(t: &ToolCall) -> String {
    let running = is_running(t);
    let pick = |done: &str, doing: &str| if running { doing } else { done }.to_string();
    if t.status == ToolStatus::Error {
        return "Failed".into();
    }
    match kind(&t.name) {
        Kind::Ran => pick("Ran", "Running"),
        Kind::Read => pick("Read", "Reading"),
        Kind::Edited if t.name == "write" => pick("Wrote", "Writing"),
        Kind::Edited => pick("Edited", "Editing"),
        Kind::Searched if t.name == "webfetch" => pick("Fetched", "Fetching"),
        Kind::Searched => pick("Searched", "Searching"),
        Kind::Other => pick("Used", "Using"),
    }
}

/// What the collapsed row names: the command, path, pattern or tool.
fn row_summary(t: &ToolCall) -> String {
    let keys: &[&str] = match kind(&t.name) {
        Kind::Ran => &["command", "description"],
        Kind::Searched => &["pattern", "query", "url", "path"],
        Kind::Other => &["description", "filePath", "path", "name"],
        _ => &["filePath", "path"],
    };
    let found = keys
        .iter()
        .find_map(|k| str_input(t, k))
        .map(first_line)
        .filter(|s| !s.is_empty());
    match (kind(&t.name), found) {
        (Kind::Other, Some(s)) => format!("{} · {s}", display_name(&t.name)),
        (Kind::Other, None) => display_name(&t.name),
        (_, Some(s)) => s,
        (_, None) => display_name(&t.name),
    }
}

/// The one argument that says what a call did, for tools whose input is
/// otherwise shown as JSON.
fn main_argument(t: &ToolCall) -> Option<String> {
    if !matches!(kind(&t.name), Kind::Other) {
        return None;
    }
    [
        "description",
        "filePath",
        "path",
        "url",
        "query",
        "name",
        "prompt",
    ]
    .iter()
    .find_map(|k| str_input(t, k))
    .map(|s| s.trim_end().to_string())
    .filter(|s| !s.is_empty())
}

/// The card's input block: the command for shells, the main argument for
/// file and search tools, pretty JSON for anything else.
fn input_text(t: &ToolCall) -> String {
    if let Some(cmd) = str_input(t, "command").filter(|_| is_shell(t)) {
        return cmd.trim_end().to_string();
    }
    let simple = match kind(&t.name) {
        Kind::Read | Kind::Edited => str_input(t, "filePath").or(str_input(t, "path")),
        Kind::Searched => None,
        _ => None,
    };
    if let Some(s) = simple {
        return s.to_string();
    }
    if let Kind::Searched = kind(&t.name) {
        let main = ["pattern", "query", "url"]
            .iter()
            .find_map(|k| str_input(t, k));
        if let Some(main) = main {
            return match str_input(t, "path") {
                Some(path) => format!("{main}  in {path}"),
                None => main.to_string(),
            };
        }
    }
    serde_json::to_string_pretty(&t.input).unwrap_or_default()
}

/// "shell" → "Bash"-style display names: capitalized, with a few renames.
fn display_name(name: &str) -> String {
    match name {
        "shell" | "bash" => "Shell".into(),
        "webfetch" => "Web fetch".into(),
        "websearch" => "Web search".into(),
        "todowrite" => "Todo list".into(),
        _ => {
            let mut c = name.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
    }
}

/// "Ran 2 commands, edited 3 files and 2 more". Commands and edits rank
/// above reads and searches; at most two categories are named.
pub fn tools_summary<'a>(tools: impl IntoIterator<Item = &'a ToolCall>) -> Option<String> {
    let mut counts = [0usize; 5];
    for t in tools {
        let slot = match kind(&t.name) {
            Kind::Ran => 0,
            Kind::Edited => 1,
            Kind::Read => 2,
            Kind::Searched => 3,
            Kind::Other => 4,
        };
        counts[slot] += 1;
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let named: Vec<(usize, String)> = [
        (counts[0], "ran", "command", "commands"),
        (counts[1], "edited", "file", "files"),
        (counts[2], "read", "file", "files"),
        (counts[3], "searched", "time", "times"),
        (counts[4], "used", "tool", "tools"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, verb, one, many)| (n, format!("{verb} {}", plural(n, one, many))))
    .collect();
    if named.is_empty() {
        return None;
    }
    let rest: usize = named.iter().skip(2).map(|(n, _)| n).sum();
    let mut s = named
        .iter()
        .take(2)
        .map(|(_, p)| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if rest > 0 {
        s.push_str(&format!(" and {rest} more"));
    }
    // Capitalize the first verb only.
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some(s)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Command,
    Arg,
    Flag,
    Str,
    Var,
    Op,
    Space,
}

/// A small shell tokenizer for coloring commands: the first word of each
/// pipeline segment is the command, operators split segments, quotes and
/// `$VARS` get their own color. Not a parser; it only needs to look right.
fn shell_tokens(src: &str) -> Vec<(std::ops::Range<usize>, Tok)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut expect_cmd = true;
    while i < b.len() {
        let start = i;
        let c = b[i];
        if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                if b[i] == b'\n' {
                    expect_cmd = true;
                }
                i += 1;
            }
            out.push((start..i, Tok::Space));
            continue;
        }
        if matches!(c, b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>') {
            while i < b.len() && matches!(b[i], b'|' | b'&' | b';' | b'<' | b'>') {
                i += 1;
            }
            if i == start {
                i += 1;
            }
            let op = &src[start..i];
            // Redirections take a file name, not a new command.
            expect_cmd = !op.contains('<') && !op.contains('>');
            out.push((start..i, Tok::Op));
            continue;
        }
        if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() && b[i] != c {
                if c == b'"' && b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push((start..i, Tok::Str));
            expect_cmd = false;
            continue;
        }
        if c == b'$' {
            i += 1;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'{' | b'}'))
            {
                i += 1;
            }
            out.push((start..i, Tok::Var));
            expect_cmd = false;
            continue;
        }
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && !matches!(
                b[i],
                b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>' | b'\'' | b'"'
            )
        {
            if b[i] == b'\\' {
                i += 1;
            }
            i += 1;
        }
        i = i.min(b.len());
        let word = &src[start..i];
        // `FOO=bar cmd`: assignments don't consume the command slot.
        let assignment = expect_cmd && word.contains('=') && !word.starts_with('=');
        let tok = if expect_cmd && !assignment {
            Tok::Command
        } else if word.starts_with('-') {
            Tok::Flag
        } else {
            Tok::Arg
        };
        if !assignment {
            expect_cmd = false;
        }
        out.push((start..i, tok));
    }
    out
}

fn shell_highlight(src: &str, cx: &App) -> StyledText {
    let theme = cx.theme();
    let highlights = shell_tokens(src).into_iter().filter_map(|(range, tok)| {
        let color = match tok {
            Tok::Command => theme.blue,
            Tok::Arg | Tok::Str => theme.green,
            Tok::Flag => theme.cyan,
            Tok::Var => theme.yellow,
            Tok::Op => theme.foreground,
            Tok::Space => return None,
        };
        Some((
            range,
            HighlightStyle {
                color: Some(color),
                ..Default::default()
            },
        ))
    });
    StyledText::new(src.to_string()).with_highlights(highlights)
}

/// Titled mono text with a copy button (permission previews).
pub fn text_panel(id: &str, title: &str, text: &str, cx: &App) -> AnyElement {
    let copy = text.to_string();
    let preview: String = text.chars().take(PREVIEW_CHARS).collect();
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

#[cfg(test)]
mod tests {
    use super::{Tok, ToolCall, ToolStatus, shell_tokens, tools_summary};

    fn kinds(src: &str) -> Vec<(&str, Tok)> {
        shell_tokens(src)
            .into_iter()
            .filter(|(_, t)| *t != Tok::Space)
            .map(|(r, t)| (&src[r], t))
            .collect()
    }

    #[test]
    fn colors_pipelines() {
        assert_eq!(
            kinds("git status --short | head; FOO=1 cargo test -q > out.txt"),
            vec![
                ("git", Tok::Command),
                ("status", Tok::Arg),
                ("--short", Tok::Flag),
                ("|", Tok::Op),
                ("head", Tok::Command),
                (";", Tok::Op),
                ("FOO=1", Tok::Arg),
                ("cargo", Tok::Command),
                ("test", Tok::Arg),
                ("-q", Tok::Flag),
                (">", Tok::Op),
                ("out.txt", Tok::Arg),
            ]
        );
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: name.into(),
            name: name.into(),
            status: ToolStatus::Completed,
            input: serde_json::Value::Null,
            output: String::new(),
            error: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn summary_ranks_commands_and_edits_and_caps_categories() {
        let tools = [
            call("read"),
            call("grep"),
            call("edit"),
            call("shell"),
            call("edit"),
        ];
        assert_eq!(
            tools_summary(tools.iter()).as_deref(),
            Some("Ran 1 command, edited 2 files and 2 more")
        );
        assert_eq!(
            tools_summary([call("read")].iter()).as_deref(),
            Some("Read 1 file")
        );
    }

    #[test]
    fn quotes_and_vars() {
        assert_eq!(
            kinds(r#"echo "a \" b" $HOME 'x'"#),
            vec![
                ("echo", Tok::Command),
                (r#""a \" b""#, Tok::Str),
                ("$HOME", Tok::Var),
                ("'x'", Tok::Str),
            ]
        );
    }
}
