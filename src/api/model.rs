//! Our own view of opencode's v2 data model.
//!
//! Parsed leniently from `serde_json::Value`: the v2 API is still evolving
//! (it ships alongside v1 in 1.19.x and changes between dev builds), so an
//! unknown field or message type must never make us drop a whole frame.

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
    /// The title to show. opencode v2 doesn't generate titles yet (v1 did),
    /// so untouched sessions are called "New session - <ISO timestamp>".
    pub fn display_title(&self) -> String {
        if self.title.is_empty() || self.title.starts_with("New session - ") {
            "New session".to_string()
        } else {
            self.title.clone()
        }
    }

    /// v2 `SessionV2Info` (`location.directory`) or v1 `Session` (`directory`),
    /// which is what `session.created`/`session.updated` still carry.
    pub fn from_json(v: &Value) -> Option<Self> {
        let directory = v
            .pointer("/location/directory")
            .or_else(|| v.get("directory"))
            .and_then(Value::as_str)?;
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
    Reasoning,
    Text,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Reasoning or text; `id` is per message ("text-0", "reasoning-1").
    Prose {
        kind: BlockKind,
        id: String,
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
                let blocks = v
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(Block::from_v2_json).collect())
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
    fn from_v2_json(v: &Value) -> Option<Self> {
        let kind = match v.get("type")?.as_str()? {
            "reasoning" => BlockKind::Reasoning,
            "text" => BlockKind::Text,
            "tool" => {
                let state = v.get("state");
                let status = match state.and_then(|s| s.get("status")).and_then(Value::as_str) {
                    Some("completed") => ToolStatus::Completed,
                    Some("error") => ToolStatus::Error,
                    Some("running") => ToolStatus::Running,
                    _ => ToolStatus::Pending,
                };
                return Some(Block::Tool(ToolCall {
                    call_id: str_at(v, "/id")?,
                    name: str_at(v, "/name").unwrap_or_default(),
                    status,
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
        Some(Block::Prose {
            kind,
            id: str_at(v, "/id")?,
            text: str_at(v, "/text").unwrap_or_default(),
            done: true,
        })
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

/// First line of an opencode error object (`{message}` in v2,
/// `{name, data: {message}}` in v1), or a plain string.
pub fn error_text(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if v.is_null() {
        return None;
    }
    let msg = v
        .as_str()
        .or_else(|| v.get("message").and_then(Value::as_str))
        .or_else(|| v.pointer("/data/message").and_then(Value::as_str))
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

/// Legacy (v1-only) sessions: convert `GET /session/{id}/message` entries.
/// Sessions created through the v1 API have no v2 history, so this is how
/// they stay readable.
pub fn message_from_v1_json(v: &Value) -> Option<Message> {
    let info = v.get("info")?;
    let id = str_at(info, "/id")?;
    let parts = v
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match info.get("role")?.as_str()? {
        "user" => Some(Message::User(UserMessage {
            id,
            text: parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter(|p| p.get("synthetic").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        })),
        "assistant" => {
            let blocks = parts
                .iter()
                .filter_map(|p| {
                    let pid = str_at(p, "/id")?;
                    match p.get("type")?.as_str()? {
                        t @ ("text" | "reasoning") => Some(Block::Prose {
                            kind: if t == "text" {
                                BlockKind::Text
                            } else {
                                BlockKind::Reasoning
                            },
                            id: pid,
                            text: str_at(p, "/text").unwrap_or_default(),
                            done: true,
                        }),
                        "tool" => {
                            let state = p.get("state");
                            Some(Block::Tool(ToolCall {
                                call_id: str_at(p, "/callID").unwrap_or(pid),
                                name: str_at(p, "/tool").unwrap_or_default(),
                                status: match state
                                    .and_then(|s| s.get("status"))
                                    .and_then(Value::as_str)
                                {
                                    Some("completed") => ToolStatus::Completed,
                                    Some("error") => ToolStatus::Error,
                                    Some("running") => ToolStatus::Running,
                                    _ => ToolStatus::Pending,
                                },
                                input: state
                                    .and_then(|s| s.get("input"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                output: state
                                    .and_then(|s| s.get("output"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                error: error_text(state.and_then(|s| s.get("error"))),
                            }))
                        }
                        _ => None,
                    }
                })
                .collect();
            Some(Message::Assistant(AssistantMessage {
                id,
                blocks,
                completed: info.pointer("/time/completed").is_some(),
                finish: str_at(info, "/finish"),
                error: error_text(info.get("error")),
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_v2_message_list() {
        let raw = include_str!("../../tests/fixtures/v2-messages.json");
        let v: Value = serde_json::from_str(raw).unwrap();
        let msgs: Vec<Message> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Message::from_v2_json)
            .collect();
        assert_eq!(msgs.len(), 3);
        // The server returns newest first.
        let Message::User(u) = &msgs[2] else {
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
        assert_eq!((t.name.as_str(), t.status), ("bash", ToolStatus::Completed));
        assert!(t.output.contains("hello-from-bash"));
    }
}
