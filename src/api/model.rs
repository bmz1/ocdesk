//! Our own view of opencode 2.x's data model (`/api/*`).
//!
//! Parsed leniently from `serde_json::Value`: the API's experimental surface
//! moves between releases, so an unknown field or message type must never
//! make us drop a whole frame.

use std::path::PathBuf;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub directory: PathBuf,
    pub created: u64,
    pub updated: u64,
    pub parent_id: Option<String>,
}

impl SessionInfo {
    /// The server titles a session (`session.renamed`) shortly after its
    /// first prompt; until then it has none.
    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            "New session".to_string()
        } else {
            self.title.clone()
        }
    }

    /// `Session.Info` from `GET /api/session` / `POST /api/session`.
    pub fn from_json(v: &Value) -> Option<Self> {
        let directory = v.pointer("/location/directory").and_then(Value::as_str)?;
        Some(Self {
            id: str_at(v, "/id")?,
            title: str_at(v, "/title").unwrap_or_default(),
            directory: PathBuf::from(directory),
            created: u64_at(v, "/time/created").unwrap_or(0),
            updated: u64_at(v, "/time/updated").unwrap_or(0),
            parent_id: str_at(v, "/parentID"),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
}

impl Message {
    pub fn id(&self) -> &str {
        match self {
            Message::User(m) => &m.id,
            Message::Assistant(m) => &m.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserMessage {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AssistantMessage {
    pub id: String,
    pub blocks: Vec<Block>,
    pub completed: bool,
    pub finish: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Reasoning = 0,
    Text = 1,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Reasoning or text. `ordinal` counts blocks of the same kind within
    /// the message (the server's own key for them).
    Prose {
        kind: BlockKind,
        ordinal: u64,
        text: String,
        done: bool,
    },
    Tool(ToolCall),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub status: ToolStatus,
    pub input: Value,
    pub output: String,
    pub error: Option<String>,
}

impl Message {
    /// One entry of `GET /api/session/{id}/message` (`SessionMessage`).
    /// Returns `None` for kinds we don't render yet (agent/model switches,
    /// shell, compaction, synthetic, system).
    pub fn from_v2_json(v: &Value) -> Option<Self> {
        let id = str_at(v, "/id")?;
        match v.get("type")?.as_str()? {
            "user" => Some(Message::User(UserMessage {
                id,
                text: str_at(v, "/text").unwrap_or_default(),
            })),
            "assistant" => {
                let mut ordinals = [0u64; 2];
                let blocks = v
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .filter_map(|b| Block::from_v2_json(b, &mut ordinals))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(Message::Assistant(AssistantMessage {
                    id,
                    blocks,
                    completed: v.pointer("/time/completed").is_some(),
                    finish: str_at(v, "/finish"),
                    error: error_text(v.get("error")),
                }))
            }
            _ => None,
        }
    }
}

impl Block {
    /// `ordinals` counts reasoning/text blocks seen so far in the message;
    /// the projection has no ids for them, the event stream uses ordinals.
    fn from_v2_json(v: &Value, ordinals: &mut [u64; 2]) -> Option<Self> {
        let kind = match v.get("type")?.as_str()? {
            "reasoning" => BlockKind::Reasoning,
            "text" => BlockKind::Text,
            "tool" => {
                let state = v.get("state");
                return Some(Block::Tool(ToolCall {
                    call_id: str_at(v, "/id")?,
                    name: str_at(v, "/name").unwrap_or_default(),
                    status: tool_status(
                        state.and_then(|s| s.get("status")).and_then(Value::as_str),
                    ),
                    input: state
                        .and_then(|s| s.get("input"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    output: content_text(state.and_then(|s| s.get("content"))),
                    error: error_text(state.and_then(|s| s.get("error"))),
                }));
            }
            _ => return None,
        };
        let slot = &mut ordinals[kind as usize];
        let ordinal = *slot;
        *slot += 1;
        Some(Block::Prose {
            kind,
            ordinal,
            text: str_at(v, "/text").unwrap_or_default(),
            done: true,
        })
    }
}

fn tool_status(s: Option<&str>) -> ToolStatus {
    match s {
        Some("completed") => ToolStatus::Completed,
        Some("error") => ToolStatus::Error,
        Some("running") => ToolStatus::Running,
        _ => ToolStatus::Pending,
    }
}

/// Concatenate the `text` items of a v2 content array.
pub fn content_text(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// First line of an opencode error object (`{type, message}`), or a plain
/// string.
pub fn error_text(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if v.is_null() {
        return None;
    }
    let msg = v
        .as_str()
        .or_else(|| v.get("message").and_then(Value::as_str))
        .or_else(|| v.get("name").and_then(Value::as_str))
        .unwrap_or("error");
    Some(msg.lines().next().unwrap_or_default().to_string())
}

pub fn str_at(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Value::as_str).map(str::to_string)
}

pub fn u64_at(v: &Value, ptr: &str) -> Option<u64> {
    v.pointer(ptr)
        .and_then(|n| n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_message_list() {
        let raw = include_str!("../../tests/fixtures/v2-messages.json");
        let v: Value = serde_json::from_str(raw).unwrap();
        let msgs: Vec<Message> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Message::from_v2_json)
            .collect();
        // user, assistant (tool step), assistant, user, assistant (interrupted); idle entries skipped.
        assert_eq!(msgs.len(), 5);
        let Message::User(u) = &msgs[0] else {
            panic!("expected user")
        };
        assert_eq!(u.text, "tool:bash please");
        let Message::Assistant(a) = &msgs[1] else {
            panic!("expected assistant")
        };
        assert_eq!(a.finish.as_deref(), Some("tool-calls"));
        let Block::Tool(t) = &a.blocks[2] else {
            panic!("expected tool")
        };
        assert_eq!(
            (t.name.as_str(), t.status),
            ("shell", ToolStatus::Completed)
        );
        assert!(t.output.contains("hello-from-bash"));
        let Message::Assistant(stopped) = &msgs[4] else {
            panic!("expected assistant")
        };
        assert_eq!(stopped.error.as_deref(), Some("Step interrupted"));
    }
}
