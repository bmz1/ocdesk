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
    let mut history: Vec<Message> = (0..turns).flat_map(turn).collect();
    history.extend(showcase(now));
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
        seed_showcase_state(s, cx);
    });
    if let Ok(prompt) = std::env::var("OCDESK_DEMO_AUTOPLAY") {
        stream_reply(store.clone(), prompt, cx);
    }
}

/// Turns that show the transcript's other states: a work log with many
/// tools, a plan-agent turn, and a failed step.
fn showcase(now: u64) -> Vec<Message> {
    use crate::api::model::{Meta, ModelRef, Timing, UserMessage};
    let tool = |id: &str, name: &str, input: serde_json::Value, out: &str, status| {
        Block::Tool(Box::new(ToolCall {
            call_id: id.into(),
            name: name.into(),
            status,
            input,
            output: out.into(),
            error: None,
            metadata: serde_json::Value::Null,
        }))
    };
    let prose = |kind, ordinal, text: &str, secs: u64| Block::Prose {
        kind,
        ordinal,
        text: text.into(),
        done: true,
        time: Timing {
            created: Some(now - 60_000),
            completed: Some(now - 60_000 + secs * 1000),
        },
    };
    let meta = |agent: &str, secs: u64| Meta {
        created: Some(now - 60_000),
        completed: Some(now - 60_000 + secs * 1000),
        model: Some(ModelRef {
            provider_id: "anthropic".into(),
            id: "claude-opus-5-5".into(),
            variant: Some("high".into()),
        }),
        agent: Some(agent.into()),
        tokens: Some(crate::api::features::Tokens {
            input: 41_200,
            output: 2_300,
            ..Default::default()
        }),
        cost: Some(0.42),
    };
    let user = |id: &str, text: &str| {
        Message::User(UserMessage {
            id: id.into(),
            text: text.into(),
            attachments: vec![],
            meta: Meta {
                created: Some(now - 60_000),
                ..Default::default()
            },
        })
    };
    use serde_json::json;
    // OCDESK_DEMO_IMAGE=/path/to.png attaches an image to the first prompt.
    let mut first = user(
        "msg_z1",
        "Find why the sidebar goes stale after sleep and fix it",
    );
    if let (Message::User(u), Ok(path)) = (&mut first, std::env::var("OCDESK_DEMO_IMAGE")) {
        u.attachments.push(crate::api::model::Attachment {
            name: "screenshot.png".into(),
            uri: Some(crate::api::model::file_uri(std::path::Path::new(&path))),
        });
    }
    vec![
        first,
        Message::Assistant(AssistantMessage {
            id: "msg_z2".into(),
            blocks: vec![
                prose(
                    BlockKind::Reasoning,
                    0,
                    "Start with how the client opens the stream.",
                    4,
                ),
                tool(
                    "c0",
                    "todowrite",
                    json!({"todos": [
                        {"content": "Find where the stream reconnects", "status": "completed"},
                        {"content": "Add a heartbeat watchdog", "status": "completed"},
                        {"content": "Fix backoff resetting on every message", "status": "in_progress"},
                        {"content": "Test a flapping server", "status": "pending"},
                        {"content": "Rewrite the stream in WebSockets", "status": "cancelled"}
                    ]}),
                    "",
                    ToolStatus::Completed,
                ),
                tool(
                    "c1",
                    "grep",
                    json!({"pattern": "EventSource", "path": "src"}),
                    "src/stream.ts:12",
                    ToolStatus::Completed,
                ),
                tool(
                    "c2",
                    "read",
                    json!({"filePath": "src/stream.ts"}),
                    "<file>\n00001| import { backoff } from \"./backoff\"\n00002| \n00003| // Opens the stream and reconnects on error.\n00004| export function open(url: string): EventSource {\n00005|   const es = new EventSource(url)\n00006|   es.onerror = () => backoff(open, 1_000)\n00007|   return es\n00008| }\n</file>",
                    ToolStatus::Completed,
                ),
                tool(
                    "c3",
                    "read",
                    json!({"filePath": "src/backoff.ts"}),
                    "…",
                    ToolStatus::Completed,
                ),
                tool(
                    "c4",
                    "shell",
                    json!({"command": "bun test stream"}),
                    "3 pass\n1 fail",
                    ToolStatus::Completed,
                ),
                tool(
                    "c5",
                    "edit",
                    json!({"filePath": "src/stream.ts"}),
                    "",
                    ToolStatus::Completed,
                ),
                prose(
                    BlockKind::Text,
                    0,
                    "The stream only reconnects on an `error` event; a half-open socket after sleep never raises one. I added a 25 s heartbeat watchdog.",
                    0,
                ),
            ],
            completed: true,
            finish: Some("stop".into()),
            error: None,
            meta: meta("build", 48),
        }),
        user("msg_z3", "Plan the retry-budget refactor"),
        Message::Assistant(AssistantMessage {
            id: "msg_z4".into(),
            blocks: vec![
                prose(BlockKind::Reasoning, 0, "List the call sites first.", 2),
                prose(BlockKind::Reasoning, 1, "Then group them by owner.", 3),
                prose(
                    BlockKind::Text,
                    0,
                    "## Plan\n\n1. Move the budget into `RetryPolicy`.\n2. Thread it through `Client::send`.\n3. Delete the per-call counters.",
                    0,
                ),
            ],
            completed: true,
            finish: Some("stop".into()),
            error: None,
            meta: meta("plan", 21),
        }),
        user("msg_z5", "Run the migration against staging"),
        Message::Assistant(AssistantMessage {
            id: "msg_z6".into(),
            blocks: vec![tool(
                "c6",
                "shell",
                json!({"command": "bun run migrate --env staging"}),
                "",
                ToolStatus::Error,
            )],
            completed: true,
            finish: Some("error".into()),
            error: Some("Provider returned 529: overloaded".into()),
            meta: meta("build", 7),
        }),
    ]
}

/// A pending approval, usage and a diff, so those surfaces show offline.
fn seed_showcase_state(s: &mut Store, cx: &mut gpui_kit::Context<Store>) {
    use crate::api::model::{FileDiff, PermissionRequest};
    if std::env::var("OCDESK_DEMO_APPROVAL").is_ok()
        && let Some(r) = PermissionRequest::from_json(&serde_json::json!({
            "id": "perm_demo", "sessionID": SESSION_ID, "action": "shell",
            "resources": ["git push --force origin main"], "save": ["git push *"],
            "metadata": {"command": "git push --force origin main"}
        }))
    {
        s.set_permissions(SESSION_ID, vec![r], cx);
    }
    s.usage.insert(
        SESSION_ID.into(),
        crate::api::features::UsageReport {
            tokens: crate::api::features::Tokens {
                input: 52_000,
                output: 12_600,
                ..Default::default()
            },
            cost: 1.84,
            context: Some(64_600),
            context_limit: Some(200_000),
            ..Default::default()
        },
    );
    s.diffs.insert(
        SESSION_ID.into(),
        vec![FileDiff {
            file: "src/stream.ts".into(),
            patch: "diff --git a/src/stream.ts b/src/stream.ts\nindex 1..2 100644\n--- a/src/stream.ts\n+++ b/src/stream.ts\n@@ -10,4 +10,8 @@ export function open() {\n   const es = new EventSource(url)\n-  es.onerror = reconnect\n+  const QUIET_MS = 25_000\n+  let timer = setTimeout(reconnect, QUIET_MS)\n+  es.onmessage = () => {\n+    clearTimeout(timer)\n+    timer = setTimeout(reconnect, QUIET_MS)\n+  }\n   return es\n }\n".into(),
            status: "modified".into(),
            additions: 5,
            deletions: 1,
            more: false,
        }],
    );
    cx.notify();
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
