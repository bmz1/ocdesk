//! The agent's todo list (`todowrite`), as opencode's own app shows it.
//!
//! opencode keeps no todo state on the server: the current list is the input
//! of the session's latest completed `todowrite` call
//! (`packages/app/src/session/composer/todos.ts`). ocdesk shows it as a
//! progress pill above the prompt that expands into the checklist, and each
//! `todowrite` call in the transcript as a checklist card.

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::api::model::{Block, Message, ToolCall, ToolStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

impl Todo {
    pub fn finished(&self) -> bool {
        matches!(self.status, TodoStatus::Completed | TodoStatus::Cancelled)
    }
}

/// `[{content, status}]`; entries without content are dropped and unknown
/// statuses read as pending.
pub fn parse(value: &Value) -> Vec<Todo> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let content = item.get("content")?.as_str()?.trim();
            if content.is_empty() {
                return None;
            }
            let status = match item.get("status").and_then(Value::as_str) {
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                Some("cancelled") => TodoStatus::Cancelled,
                _ => TodoStatus::Pending,
            };
            Some(Todo {
                content: content.to_string(),
                status,
            })
        })
        .collect()
}

/// The list a `todowrite` call set.
pub fn of_call(t: &ToolCall) -> Vec<Todo> {
    parse(t.input.get("todos").unwrap_or(&Value::Null))
}

/// The session's current list: the input of its latest completed
/// `todowrite` call.
pub fn session_todos(messages: &[Message]) -> Vec<Todo> {
    messages
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        })
        .flat_map(|a| a.blocks.iter().rev())
        .find_map(|b| match b {
            Block::Tool(t) if t.name == "todowrite" && t.status == ToolStatus::Completed => {
                Some(of_call(t))
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The todo the agent is on: in progress, else the next pending one, else
/// the last finished one.
pub fn current(todos: &[Todo]) -> Option<&Todo> {
    todos
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
        .or_else(|| todos.iter().find(|t| t.status == TodoStatus::Pending))
        .or_else(|| {
            todos
                .iter()
                .rev()
                .find(|t| t.status == TodoStatus::Completed)
        })
        .or_else(|| todos.first())
}

/// (finished, total)
pub fn progress(todos: &[Todo]) -> (usize, usize) {
    (todos.iter().filter(|t| t.finished()).count(), todos.len())
}

fn marker(status: TodoStatus, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let box_ = |border: Hsla| {
        div()
            .size(px(13.))
            .flex_none()
            .rounded(px(4.))
            .border_1()
            .border_color(border)
            .flex()
            .items_center()
            .justify_center()
    };
    match status {
        TodoStatus::Completed => box_(theme.success)
            .bg(theme.success)
            .child(
                Icon::new(IconName::Check)
                    .size(px(10.))
                    .text_color(theme.background),
            )
            .into_any_element(),
        TodoStatus::InProgress => box_(theme.primary)
            .child(div().size(px(5.)).rounded_full().bg(theme.primary))
            .into_any_element(),
        TodoStatus::Cancelled => box_(theme.muted_foreground)
            .child(
                Icon::new(IconName::Minus)
                    .size(px(10.))
                    .text_color(theme.muted_foreground),
            )
            .into_any_element(),
        TodoStatus::Pending => box_(theme.muted_foreground).into_any_element(),
    }
}

/// The checklist: one row per todo.
pub fn checklist(todos: &[Todo], cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .gap_1()
        .children(todos.iter().map(|t| {
            h_flex()
                .gap_2()
                .items_start()
                .text_sm()
                .child(div().pt(px(3.)).child(marker(t.status, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(t.finished(), |d| d.text_color(theme.muted_foreground))
                        .when(t.status == TodoStatus::Cancelled, |d| d.line_through())
                        .when(t.status == TodoStatus::InProgress, |d| {
                            d.font_weight(FontWeight::MEDIUM)
                        })
                        .child(t.content.clone()),
                )
        }))
        .into_any_element()
}

/// A thin bar for (done, total).
pub fn bar(done: usize, total: usize, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let f = if total == 0 {
        0.
    } else {
        done as f32 / total as f32
    };
    div()
        .w(px(56.))
        .h(px(4.))
        .flex_none()
        .rounded_full()
        .bg(theme.secondary)
        .child(
            div()
                .h_full()
                .w(relative(f))
                .rounded_full()
                .bg(theme.success),
        )
        .into_any_element()
}

/// The pill above the prompt: progress and the current step; expands into
/// the checklist. `None` when there is no list, or it is finished and the
/// session idle.
pub fn pill(
    todos: &[Todo],
    open: bool,
    busy: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    let (done, total) = progress(todos);
    if total == 0 || (done == total && !busy) {
        return None;
    }
    let theme = cx.theme();
    let now = current(todos)
        .map(|t| t.content.clone())
        .unwrap_or_default();
    Some(
        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.35))
            .child(
                h_flex()
                    .id("todo-pill")
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .cursor_pointer()
                    .on_click(move |_, window, cx| on_toggle(window, cx))
                    .child(crate::ui::icons::list_checks().small())
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(format!("{done}/{total}")),
                    )
                    .child(bar(done, total, cx))
                    .child(div().flex_1().min_w_0().truncate().child(now))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronUp
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                    ),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .id("todo-list")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .child(checklist(todos, cx)),
                )
            })
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::AssistantMessage;
    use serde_json::json;

    fn call(status: ToolStatus, todos: Value) -> Block {
        Block::Tool(Box::new(ToolCall {
            call_id: "c".into(),
            name: "todowrite".into(),
            status,
            input: json!({ "todos": todos }),
            output: String::new(),
            error: None,
            metadata: Value::Null,
        }))
    }

    #[::core::prelude::v1::test]
    fn latest_completed_call_wins_and_current_follows_progress() {
        let msg = |blocks| {
            Message::Assistant(AssistantMessage {
                id: "a".into(),
                blocks,
                ..Default::default()
            })
        };
        let messages = vec![
            msg(vec![call(
                ToolStatus::Completed,
                json!([{"content":"a","status":"pending"}]),
            )]),
            msg(vec![
                call(
                    ToolStatus::Completed,
                    json!([{"content":"a","status":"completed"},{"content":"b","status":"in_progress"},{"content":"","status":"pending"},{"content":"c","status":"odd"}]),
                ),
                call(ToolStatus::Running, json!([])),
            ]),
        ];
        let todos = session_todos(&messages);
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[2].status, TodoStatus::Pending);
        assert_eq!(current(&todos).unwrap().content, "b");
        assert_eq!(progress(&todos), (1, 3));
    }
}
