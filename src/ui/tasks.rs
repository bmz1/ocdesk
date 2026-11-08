//! Background tasks drawer: the selected session's subagents (child sessions
//! started by the `subagent` tool) and the shell commands the server moved
//! to the background, in one right-side panel.
//!
//! Layout follows a "task detail" panel: a header with back/close, the
//! subagent's prompt in a card (collapsed, "Show more"), its agent, a
//! one-line activity summary ("Ran 3 commands, read 2 files"), a spinner
//! while it runs, then its result. Shells show the command, status and the
//! tail of their output (polled while running), with Stop.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{Block, BlockKind, Message, SessionInfo, ShellInfo, ToolCall};
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};
use crate::ui::icons;

pub const DRAWER_W: f32 = 420.;
const POLL: Duration = Duration::from_secs(1);

/// What the drawer shows.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskView {
    List,
    Subagent(String),
    Shell(String),
}

/// Something in the transcript that opens the drawer.
pub type OpenTask = std::rc::Rc<dyn Fn(TaskView, &mut App)>;

pub struct TasksDrawer {
    store: Entity<Store>,
    controller: Entity<Controller>,
    pub view: TaskView,
    show_full_prompt: bool,
    /// Latest output per shell id.
    output: std::collections::HashMap<String, String>,
    poll: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl TasksDrawer {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![
            cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| match ev {
                StoreEvent::SelectionChanged => {
                    this.view = TaskView::List;
                    this.output.clear();
                    this.poll = None;
                    cx.notify();
                }
                StoreEvent::TasksChanged
                | StoreEvent::MessagesReset { .. }
                | StoreEvent::MessageInserted { .. }
                | StoreEvent::MessageChanged { .. }
                | StoreEvent::StatusChanged { .. } => cx.notify(),
                _ => {}
            }),
        ];
        Self {
            store,
            controller,
            view: TaskView::List,
            show_full_prompt: false,
            output: Default::default(),
            poll: None,
            _subs: subs,
        }
    }

    /// Show `view`, loading what it needs.
    pub fn show(&mut self, view: TaskView, cx: &mut Context<Self>) {
        self.show_full_prompt = false;
        self.poll = None;
        match &view {
            TaskView::List => {
                if let Some(sid) = self.store.read(cx).selected.clone() {
                    self.controller.update(cx, |c, cx| c.refresh_tasks(sid, cx));
                }
            }
            TaskView::Subagent(child) => {
                let child = child.clone();
                self.controller.update(cx, |c, cx| c.load_child(child, cx));
            }
            TaskView::Shell(id) => self.start_polling(id.clone(), cx),
        }
        self.view = view;
        cx.notify();
    }

    /// Fetch the shell's output now and every second while it runs.
    fn start_polling(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(dir) = self.session_dir(cx) else {
            return;
        };
        let controller = self.controller.clone();
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let Some(rx) =
                    controller.update(cx, |c, _| c.shell_output(dir.clone(), id.clone()))
                else {
                    return;
                };
                if let Ok(Ok(text)) = rx.await {
                    let running = this
                        .update(cx, |this, cx| {
                            this.output.insert(id.clone(), text);
                            cx.notify();
                            this.store
                                .read(cx)
                                .shells
                                .get(&id)
                                .is_some_and(ShellInfo::running)
                        })
                        .unwrap_or(false);
                    if !running {
                        return;
                    }
                }
                cx.background_executor().timer(POLL).await;
            }
        }));
    }

    fn session_dir(&self, cx: &App) -> Option<PathBuf> {
        let s = self.store.read(cx);
        s.selected
            .as_ref()
            .and_then(|id| s.session(id))
            .map(|s| s.directory.clone())
            .or_else(|| s.target_folder())
    }

    fn header(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let back = self.view != TaskView::List;
        h_flex()
            .h_11()
            .flex_none()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .when(back, |el| {
                el.child(
                    Button::new("tasks-back")
                        .icon(IconName::ChevronLeft)
                        .ghost()
                        .small()
                        .tooltip("All tasks")
                        .on_click(cx.listener(|this, _, _, cx| this.show(TaskView::List, cx))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .when(!back, |d| d.pl_2())
                    .child(title),
            )
            .child(
                Button::new("tasks-close")
                    .icon(IconName::Close)
                    .ghost()
                    .small()
                    .tooltip_with_action("Close", &super::keys::ToggleTasks, None)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(super::keys::ToggleTasks), cx)
                    }),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let Some(sid) = store.selected.clone() else {
            return div().into_any_element();
        };
        let kids: Vec<SessionInfo> = store.children_of(&sid).to_vec();
        let shells: Vec<ShellInfo> = store.shells_of(&sid).into_iter().cloned().collect();
        let busy: Vec<bool> = kids.iter().map(|k| store.is_busy(&k.id)).collect();
        if kids.is_empty() && shells.is_empty() {
            return div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No background tasks in this session. Subagents and long-running shell commands appear here.")
                .into_any_element();
        }
        let section = |label: &'static str| {
            div()
                .px_3()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let row = |id: SharedString, icon: Icon, title: String, detail: String, running: bool| {
            h_flex()
                .id(id)
                .mx_1()
                .px_2()
                .py_1p5()
                .gap_2()
                .rounded_lg()
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary))
                .child(icon.small().text_color(theme.muted_foreground))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(title))
                        .child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        ),
                )
                .when(running, |el| {
                    el.child(Spinner::new().xsmall().color(theme.muted_foreground))
                })
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
        };
        let mut col = v_flex().pb_2();
        if !kids.is_empty() {
            col = col.child(section("Subagents"));
            for (k, running) in kids.iter().zip(busy) {
                let id = k.id.clone();
                let detail = format!(
                    "{} · {}",
                    k.agent.clone().unwrap_or_else(|| "subagent".into()),
                    if running { "running" } else { "done" }
                );
                col = col.child(
                    row(
                        format!("task-{id}").into(),
                        icons::workflow(),
                        k.display_title(),
                        detail,
                        running,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show(TaskView::Subagent(id.clone()), cx)
                    })),
                );
            }
        }
        if !shells.is_empty() {
            col = col.child(section("Shells"));
            for sh in shells {
                let id = sh.id.clone();
                col =
                    col.child(
                        row(
                            format!("task-{id}").into(),
                            icons::terminal(),
                            sh.command.clone(),
                            shell_status(&sh),
                            sh.running(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show(TaskView::Shell(id.clone()), cx)
                        })),
                    );
            }
        }
        col.into_any_element()
    }

    fn render_subagent(&self, child: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let parent = store.selected.clone().unwrap_or_default();
        let info = store
            .children_of(&parent)
            .iter()
            .find(|k| k.id == child)
            .cloned();
        let running = store.is_busy(child);
        let call = parent_call(store, &parent, child);
        let prompt = call
            .as_ref()
            .and_then(|t| t.input.get("prompt").and_then(|p| p.as_str()))
            .unwrap_or_default()
            .to_string();
        let messages: Vec<Message> = store
            .conversations
            .get(child)
            .map(|c| c.messages.clone())
            .unwrap_or_default();
        let summary = activity_summary(&messages);
        let result = last_text(&messages).or_else(|| {
            call.as_ref()
                .filter(|t| !t.output.is_empty())
                .map(|t| strip_subagent_wrapper(&t.output))
        });
        let agent = info
            .as_ref()
            .and_then(|i| i.agent.clone())
            .or_else(|| {
                call.as_ref()
                    .and_then(|t| t.input.get("agent")?.as_str().map(str::to_string))
            })
            .unwrap_or_else(|| "subagent".into());
        let long_prompt = prompt.lines().count() > 4 || prompt.len() > 320;
        let show_full = self.show_full_prompt;

        v_flex()
            .p_3()
            .gap_3()
            .when(!prompt.is_empty(), |el| {
                el.child(
                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded_xl()
                        .bg(theme.secondary.opacity(0.6))
                        .text_sm()
                        .child(
                            div()
                                .when(!show_full, |d| d.line_clamp(4))
                                .whitespace_normal()
                                .child(prompt.clone()),
                        )
                        .when(long_prompt, |el| {
                            el.child(
                                div()
                                    .id("prompt-more")
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.foreground))
                                    .child(if show_full { "Show less" } else { "Show more" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_full_prompt = !this.show_full_prompt;
                                        cx.notify();
                                    })),
                            )
                        }),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Agent {agent}")),
            )
            .when_some(summary, |el, s| {
                el.child(
                    h_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(s),
                )
            })
            .when(running, |el| {
                el.child(Spinner::new().small().color(theme.primary))
            })
            .when_some(result, |el, text| {
                el.child(div().text_sm().whitespace_normal().child(text))
            })
            .into_any_element()
    }

    fn render_shell(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(sh) = self.store.read(cx).shells.get(id).cloned() else {
            return div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("This command is gone.")
                .into_any_element();
        };
        let output = self.output.get(id).cloned().unwrap_or_default();
        let dir = self.session_dir(cx);
        let kill_id = sh.id.clone();
        v_flex()
            .p_3()
            .gap_3()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.secondary.opacity(0.6))
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .whitespace_normal()
                    .child(format!("$ {}", sh.command)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(sh.running(), |el| {
                        el.child(Spinner::new().xsmall().color(theme.muted_foreground))
                    })
                    .child(div().flex_1().child(shell_status(&sh)))
                    .when(sh.running() && dir.is_some(), |el| {
                        let dir = dir.clone().unwrap_or_default();
                        el.child(
                            Button::new("shell-kill")
                                .label("Stop")
                                .icon(IconName::Square)
                                .xsmall()
                                .outline()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let (d, i) = (dir.clone(), kill_id.clone());
                                    this.controller.update(cx, |c, cx| c.kill_shell(d, i, cx));
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .whitespace_normal()
                    .text_color(if output.is_empty() {
                        theme.muted_foreground
                    } else {
                        theme.foreground
                    })
                    .child(if output.is_empty() {
                        "No output yet".to_string()
                    } else {
                        tail(&output, 200)
                    }),
            )
            .into_any_element()
    }
}

impl Render for TasksDrawer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (title, body) = match self.view.clone() {
            TaskView::List => ("Background tasks".to_string(), self.render_list(cx)),
            TaskView::Subagent(child) => {
                let store = self.store.read(cx);
                let parent = store.selected.clone().unwrap_or_default();
                let title = store
                    .children_of(&parent)
                    .iter()
                    .find(|k| k.id == child)
                    .map(SessionInfo::display_title)
                    .unwrap_or_else(|| "Subagent".into());
                (title, self.render_subagent(&child, cx))
            }
            TaskView::Shell(id) => {
                let title = self
                    .store
                    .read(cx)
                    .shells
                    .get(&id)
                    .map(|s| s.command.clone())
                    .unwrap_or_else(|| "Shell".into());
                (title, self.render_shell(&id, cx))
            }
        };
        v_flex()
            .id("tasks-drawer")
            .w(px(DRAWER_W))
            .h_full()
            .flex_none()
            // The drawer can float over the conversation: keep wheel and
            // mouse events from reaching the message list behind it.
            .occlude()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(self.header(title, cx))
            .child(
                div()
                    .id("tasks-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

/// "running · 12s", "exited 0", "killed".
fn shell_status(sh: &ShellInfo) -> String {
    match (sh.status.as_str(), sh.exit) {
        ("running", _) => "running in the background".into(),
        ("exited", Some(code)) => format!("exited with {code}"),
        (status, _) => status.to_string(),
    }
}

/// The parent's `subagent` tool call that started `child`.
pub fn parent_call(store: &Store, parent: &str, child: &str) -> Option<ToolCall> {
    let title = store
        .children_of(parent)
        .iter()
        .find(|k| k.id == child)
        .map(|k| k.title.clone());
    store
        .conversations
        .get(parent)?
        .messages
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        })
        .flat_map(|a| a.blocks.iter())
        .filter_map(|b| match b {
            Block::Tool(t) if t.name == "subagent" => Some(t.as_ref()),
            _ => None,
        })
        .find(|t| {
            subagent_session(t).as_deref() == Some(child)
                || (subagent_session(t).is_none()
                    && title.as_deref() == t.input.get("description").and_then(|d| d.as_str()))
        })
        .cloned()
}

/// The child session id a `subagent` tool call reports (`metadata.sessionID`).
pub fn subagent_session(t: &ToolCall) -> Option<String> {
    t.metadata
        .get("sessionID")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The shell id of a tool call the server moved to the background.
pub fn background_shell(t: &ToolCall) -> Option<String> {
    t.metadata
        .get("shellID")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// "Ran 3 commands, read 2 files, edited 1 file" from a child transcript.
pub fn activity_summary(messages: &[Message]) -> Option<String> {
    let mut counts = [0usize; 4]; // ran, read, edited, searched
    for m in messages {
        if let Message::Assistant(a) = m {
            for b in &a.blocks {
                if let Block::Tool(t) = b {
                    let slot = match t.name.as_str() {
                        "shell" | "bash" | "execute" => 0,
                        "read" => 1,
                        "edit" | "write" | "patch" => 2,
                        "grep" | "glob" | "list" | "websearch" | "webfetch" => 3,
                        _ => continue,
                    };
                    counts[slot] += 1;
                }
            }
        }
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let parts: Vec<String> = [
        (counts[0], "Ran", "command", "commands"),
        (counts[1], "read", "file", "files"),
        (counts[2], "edited", "file", "files"),
        (counts[3], "searched", "time", "times"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, verb, one, many)| format!("{verb} {}", plural(n, one, many)))
    .collect();
    if parts.is_empty() {
        return None;
    }
    let mut s = parts.join(", ");
    // Capitalize the first verb only.
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some(s)
}

/// The child's final answer: the last assistant text.
fn last_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => {
            let text: Vec<&str> = a
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Prose {
                        kind: BlockKind::Text,
                        text,
                        ..
                    } if !text.trim().is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then(|| text.join("\n\n"))
        }
        _ => None,
    })
}

/// The tool result wraps the answer in `<subagent …>…</subagent>`.
fn strip_subagent_wrapper(text: &str) -> String {
    let inner = text
        .split_once('>')
        .filter(|(head, _)| head.trim_start().starts_with("<subagent"))
        .map_or(text, |(_, rest)| rest);
    inner
        .trim_end()
        .trim_end_matches("</subagent>")
        .trim()
        .to_string()
}

/// The last `n` lines.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's `test` attribute in via `gpui_kit::*`.
    use super::{activity_summary, strip_subagent_wrapper};
    use crate::api::model::{AssistantMessage, Block, Message, ToolCall, ToolStatus};

    fn tool(name: &str) -> Block {
        Block::Tool(Box::new(ToolCall {
            call_id: "c".into(),
            name: name.into(),
            status: ToolStatus::Completed,
            input: serde_json::json!({}),
            output: String::new(),
            error: None,
            metadata: serde_json::json!({}),
        }))
    }

    #[test]
    fn summarizes_child_activity() {
        let msg = Message::Assistant(AssistantMessage {
            id: "a".into(),
            blocks: vec![tool("shell"), tool("shell"), tool("read"), tool("grep")],
            ..Default::default()
        });
        assert_eq!(
            activity_summary(&[msg]).as_deref(),
            Some("Ran 2 commands, read 1 file, searched 1 time")
        );
        assert_eq!(activity_summary(&[]), None);
    }

    #[test]
    fn unwraps_subagent_result() {
        let out = "<subagent sessionID=\"ses_c\" state=\"completed\">\nHello there\n</subagent>";
        assert_eq!(strip_subagent_wrapper(out), "Hello there");
        assert_eq!(strip_subagent_wrapper("plain"), "plain");
    }
}
