//! Offline UI demo (`OCDESK_DEMO`): no sidecar, no model.
//!
//! Seeds the store with one session of synthetic history and streams
//! replies to prompts through the same `Store::apply` path real events take,
//! word by word. Built for UI and perf work where opencode isn't available.
//!
//! - `OCDESK_DEMO=<n>`: number of history turns (user + assistant), default 20.
//!   `OCDESK_DEMO=250` gives the 500-message perf case.
//! - `OCDESK_DEMO_DELAY_MS`: delay between streamed words, default 15.
//! - `OCDESK_DEMO_AUTOPLAY=<prompt>`: send this prompt once at startup.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{App, Entity};

use crate::api::model::{
    AssistantMessage, Block, BlockKind, Message, ToolCall, ToolStatus, UserMessage,
};
use crate::api::{Envelope, Event, events::Outcome};
use crate::store::{Connection, Store};

pub const SESSION_ID: &str = "ses_demo";

/// History turns requested via `OCDESK_DEMO`, if demo mode is on.
pub fn turns() -> Option<usize> {
    let v = std::env::var("OCDESK_DEMO").ok()?;
    Some(v.parse().unwrap_or(20))
}

pub fn start(store: &Entity<Store>, turns: usize, cx: &mut App) {
    let folder = store
        .read(cx)
        .folders
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/demo"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let session = crate::api::model::SessionInfo {
        id: SESSION_ID.into(),
        title: format!("Markdown demo ({} messages)", turns * 2),
        directory: folder.clone(),
        created: now,
        updated: now,
        parent_id: None,
        model: None,
        agent: None,
    };
    let history: Vec<Message> = (0..turns).flat_map(turn).collect();
    tracing::info!(messages = history.len(), "demo mode: seeded history");
    store.update(cx, |s, cx| {
        if s.folders.is_empty() {
            s.set_folders(vec![folder], cx);
        }
        s.opencode_version = Some("demo".into());
        s.set_connection(Connection::Connected, cx);
        s.set_sessions(vec![session], cx);
        s.reconcile(SESSION_ID, history, cx);
        s.select(Some(SESSION_ID.into()), cx);
    });
    if let Ok(prompt) = std::env::var("OCDESK_DEMO_AUTOPLAY") {
        stream_reply(store.clone(), prompt, cx);
    }
}

fn id(prefix: &str, n: usize) -> String {
    format!("{prefix}_{n:08}")
}

/// One synthetic history turn: a prompt and a finished reply.
fn turn(i: usize) -> [Message; 2] {
    let (prompt, reasoning, answer) = sample(i);
    let mut blocks = vec![Block::Prose {
        kind: BlockKind::Reasoning,
        ordinal: 0,
        text: reasoning.into(),
        done: true,
        time: Default::default(),
    }];
    if i % 3 == 1 {
        blocks.push(Block::Tool(Box::new(ToolCall {
            call_id: format!("call_{i}"),
            name: "shell".into(),
            status: ToolStatus::Completed,
            input: serde_json::json!({ "command": "cargo test --quiet" }),
            output: "test result: ok".into(),
            error: None,
            metadata: serde_json::Value::Null,
        })));
    }
    blocks.push(Block::Prose {
        kind: BlockKind::Text,
        ordinal: 0,
        text: answer,
        done: true,
        time: Default::default(),
    });
    [
        Message::User(UserMessage {
            id: id("msg", 2 * i),
            text: prompt.into(),
            attachments: vec![],
            meta: Default::default(),
        }),
        Message::Assistant(AssistantMessage {
            id: id("msg", 2 * i + 1),
            blocks,
            completed: true,
            finish: Some("stop".into()),
            error: None,
            meta: Default::default(),
        }),
    ]
}

fn sample(i: usize) -> (&'static str, &'static str, String) {
    match i % 4 {
        0 => (
            "Show me every markdown construct",
            "The user wants **all** the constructs: lists, a table, quotes and code.",
            format!("### Answer {i}\n\n{RICH}"),
        ),
        1 => (
            "Run the tests and explain the result",
            "I'll run `cargo test` and summarize.",
            format!(
                "All tests passed (turn {i}). The relevant code:\n\n```rust\nfn add(a: i32, b: i32) -> i32 {{\n    a + b\n}}\n```\n\nSee [the docs](https://doc.rust-lang.org/book/) for more."
            ),
        ),
        2 => (
            "Summarize in one paragraph",
            "Short answer.",
            format!(
                "Turn {i}: a single paragraph with *emphasis*, **strong text**, and `inline code` that is long enough to wrap across several lines in the conversation column so line breaking gets exercised too."
            ),
        ),
        _ => (
            "Compare the options in a table",
            "Tabulate.",
            "| Option | Speed | Notes |\n|---|---|---|\n| A | fast | simple |\n| B | slow | flexible |\n| C | medium | `default` |\n\n> Pick **A** unless you need flexibility.".into(),
        ),
    }
}

const RICH: &str = "Plain text with **bold**, *italic*, ~~strike~~, `inline code` and a [link](https://opencode.ai).

> A blockquote with `code` inside.

1. First
2. Second, with a nested list:
   - nested bullet
   - another
3. Third

- [x] done task
- [ ] open task

| Column | Type | Notes |
|---|---|---|
| `id` | string | primary key |
| `created` | u64 | ms since epoch |

```rust
fn main() {
    println!(\"hello from rust\");
}
```

```python
def greet(name: str) -> str:
    return f\"hello {name}\"
```
";

const LONG_REASONING: &str = "Let me think about how to structure this. First the overview, then the details with **code**, then a short summary.\n\n- consider edge cases\n- keep it readable\n";

fn reply_text(prompt: &str) -> String {
    if prompt.contains("long") {
        (1..=8)
            .map(|i| {
                format!(
                    "## Section {i}\n\nSome *emphasis*, some `inline code`, and a [link](https://opencode.ai).\n\n```rust\nfn section_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n\n- item one\n- item two\n\n"
                )
            })
            .collect()
    } else {
        format!("You said: *{}*\n\n{RICH}", prompt.trim())
    }
}

/// Split into streamable chunks of one word plus trailing whitespace.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if ch == ' ' || ch == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Stream a reply to `prompt` into the demo session.
pub fn stream_reply(store: Entity<Store>, prompt: String, cx: &mut App) {
    let delay = Duration::from_millis(
        std::env::var("OCDESK_DEMO_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
    );
    let n = store
        .read(cx)
        .conversations
        .get(SESSION_ID)
        .map_or(0, |c| c.messages.len());
    let (user_id, msg_id) = (id("msg", n), id("msg", n + 1));
    let sid = || SESSION_ID.to_string();
    let apply = |store: &Entity<Store>, e: Event, cx: &mut App| {
        store.update(cx, |s, cx| s.apply(Envelope::bare(e), cx));
    };
    apply(&store, Event::ExecutionStarted { session_id: sid() }, cx);
    apply(
        &store,
        Event::UserPrompt {
            session_id: sid(),
            message_id: user_id,
            text: prompt.clone(),
            attachments: vec![],
        },
        cx,
    );
    apply(
        &store,
        Event::StepStarted {
            session_id: sid(),
            message_id: msg_id.clone(),
            model: None,
            agent: None,
        },
        cx,
    );
    let blocks = [
        (BlockKind::Reasoning, LONG_REASONING.to_string()),
        (BlockKind::Text, reply_text(&prompt)),
    ];
    cx.spawn(async move |cx| {
        for (kind, text) in blocks {
            let message_id = msg_id.clone();
            cx.update(|cx| {
                apply(
                    &store,
                    Event::BlockStarted {
                        session_id: sid(),
                        message_id: message_id.clone(),
                        kind,
                        ordinal: 0,
                    },
                    cx,
                )
            });
            for w in words(&text) {
                cx.background_executor().timer(delay).await;
                cx.update(|cx| {
                    apply(
                        &store,
                        Event::BlockDelta {
                            session_id: sid(),
                            message_id: message_id.clone(),
                            kind,
                            ordinal: 0,
                            delta: w,
                        },
                        cx,
                    )
                });
            }
            cx.update(|cx| {
                apply(
                    &store,
                    Event::BlockEnded {
                        session_id: sid(),
                        message_id,
                        kind,
                        ordinal: 0,
                        text,
                    },
                    cx,
                )
            });
        }
        cx.update(|cx| {
            apply(
                &store,
                Event::StepEnded {
                    session_id: sid(),
                    message_id: msg_id.clone(),
                    finish: "stop".into(),
                    tokens: None,
                    cost: None,
                },
                cx,
            );
            apply(
                &store,
                Event::ExecutionEnded {
                    session_id: sid(),
                    outcome: Outcome::Succeeded,
                },
                cx,
            );
        });
        tracing::info!("demo: reply finished");
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_keep_every_byte() {
        let s = "a **b**\n\n```rust\nfn x() {}\n```";
        assert_eq!(words(s).concat(), s);
    }

    #[test]
    fn history_ids_are_sorted() {
        let msgs: Vec<Message> = (0..12).flat_map(turn).collect();
        assert!(msgs.windows(2).all(|w| w[0].id() < w[1].id()));
    }
}
