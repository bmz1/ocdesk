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
    /// Chosen with `POST /api/session/{id}/model`; `None` until the session
    /// has one (the folder's default model applies).
    pub model: Option<ModelRef>,
    /// Primary agent (`build`, `plan`, …); `None` means the server default.
    pub agent: Option<String>,
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
            model: v.get("model").and_then(ModelRef::from_json),
            agent: str_at(v, "/agent"),
        })
    }
}

/// `Model.Ref`: which model (and optional variant, e.g. reasoning effort).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider_id: String,
    pub id: String,
    /// The server reports `"default"` when none was picked.
    pub variant: Option<String>,
}

impl ModelRef {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            provider_id: str_at(v, "/providerID")?,
            id: str_at(v, "/id")?,
            variant: str_at(v, "/variant").filter(|s| s != "default"),
        })
    }

    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({ "providerID": self.provider_id, "id": self.id });
        if let Some(var) = &self.variant {
            v["variant"] = Value::from(var.clone());
        }
        v
    }

    /// Same model, ignoring the variant.
    pub fn same_model(&self, other: &ModelRef) -> bool {
        self.provider_id == other.provider_id && self.id == other.id
    }
}

/// One entry of `GET /api/model` (`Model.Info`), trimmed to what the picker needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub provider_id: String,
    pub id: String,
    pub name: String,
    /// Variant ids (`low`, `high`, …), in server order.
    pub variants: Vec<String>,
}

impl ModelInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        if v.get("enabled").and_then(Value::as_bool) == Some(false) {
            return None;
        }
        let id = str_at(v, "/id")?;
        Some(Self {
            provider_id: str_at(v, "/providerID")?,
            name: str_at(v, "/name").unwrap_or_else(|| id.clone()),
            id,
            variants: v
                .get("variants")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| str_at(x, "/id")).collect())
                .unwrap_or_default(),
        })
    }

    pub fn as_ref(&self) -> ModelRef {
        ModelRef {
            provider_id: self.provider_id.clone(),
            id: self.id.clone(),
            variant: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
}

/// One entry of `GET /api/agent` (`Agent.Info`).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub mode: AgentMode,
    pub hidden: bool,
}

impl AgentInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        let id = str_at(v, "/id")?;
        Some(Self {
            name: str_at(v, "/name").unwrap_or_else(|| id.clone()),
            id,
            mode: match v.get("mode").and_then(Value::as_str) {
                Some("subagent") => AgentMode::Subagent,
                Some("primary") => AgentMode::Primary,
                _ => AgentMode::All,
            },
            hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Can be the session's agent (the agent picker).
    pub fn is_primary(&self) -> bool {
        !self.hidden && self.mode != AgentMode::Subagent
    }

    /// Can be @-mentioned to delegate to (`agents` prompt attachment).
    pub fn is_subagent(&self) -> bool {
        !self.hidden && self.mode != AgentMode::Primary
    }
}

/// A `GET /api/fs/find` hit, relative to the location.
#[derive(Clone, Debug, PartialEq)]
pub struct FsEntry {
    pub path: String,
    pub is_dir: bool,
}

/// Where a mention sits in the prompt text (`Prompt.Mention`). Offsets are
/// UTF-16 code units, as JavaScript string indices.
#[derive(Clone, Debug, PartialEq)]
pub struct Mention {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Mention {
    /// From a UTF-8 byte range of `text`.
    pub fn from_byte_range(text: &str, range: std::ops::Range<usize>) -> Option<Self> {
        let piece = text.get(range.clone())?;
        let start = text[..range.start].encode_utf16().count();
        Some(Self {
            start,
            end: start + piece.encode_utf16().count(),
            text: piece.to_string(),
        })
    }

    fn to_json(&self) -> Value {
        serde_json::json!({ "start": self.start, "end": self.end, "text": self.text })
    }
}

/// `POST /api/session/{id}/prompt` body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptInput {
    pub text: String,
    /// Absolute paths; the server reads and inlines them (`file://` URIs).
    pub files: Vec<(std::path::PathBuf, Mention)>,
    /// Subagent ids to delegate to.
    pub agents: Vec<(String, Mention)>,
}

impl PromptInput {
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({ "text": self.text });
        if !self.files.is_empty() {
            v["files"] = self
                .files
                .iter()
                .map(|(path, m)| {
                    serde_json::json!({
                        "uri": file_uri(path),
                        "name": m.text.trim_start_matches('@'),
                        "mention": m.to_json(),
                    })
                })
                .collect();
        }
        if !self.agents.is_empty() {
            v["agents"] = self
                .agents
                .iter()
                .map(|(name, m)| serde_json::json!({ "name": name, "mention": m.to_json() }))
                .collect();
        }
        v
    }
}

/// `file://` URI with the path percent-encoded where a URI requires it.
pub fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    Tool(Box<ToolCall>),
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
    pub metadata: Value,
}

/// Pending requests are session-owned, including while their history is unloaded.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    pub save: Vec<String>,
    pub message: Option<String>,
    pub metadata: Value,
}

impl PermissionRequest {
    pub fn from_json(v: &Value) -> Option<Self> {
        let strings = |key: &str| {
            v.get(key)
                .and_then(Value::as_array)
                .map(|xs| {
                    xs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Self {
            id: str_at(v, "/id")?,
            session_id: str_at(v, "/sessionID")?,
            action: str_at(v, "/action")?,
            resources: strings("resources"),
            save: strings("save"),
            message: str_at(v, "/message"),
            metadata: v.get("metadata").cloned().unwrap_or(Value::Null),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

/// An opencode "question" form (`Form.Info`): one blocking card asking the
/// user for answers to one or more fields. Parsed leniently like everything
/// else in this module; unknown field kinds are dropped rather than failing
/// the whole form.
#[derive(Clone, Debug, PartialEq)]
pub struct FormRequest {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub fields: Vec<FormField>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFieldKind {
    /// Free text; with `options` this is the mock's single-select.
    String,
    Number,
    Integer,
    Boolean,
    MultiSelect,
    /// A link the user must follow outside ocdesk.
    External,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormField {
    pub key: String,
    pub title: String,
    pub description: Option<String>,
    pub kind: FormFieldKind,
    pub required: bool,
    pub options: Vec<FormOption>,
    /// `custom: true` adds a free-text row besides the options.
    pub custom: bool,
    pub placeholder: Option<String>,
    pub default: Option<Value>,
    pub hidden: bool,
}

impl FormField {
    fn from_json(v: &Value) -> Option<Self> {
        let kind = match v.get("type").and_then(Value::as_str)? {
            "string" => FormFieldKind::String,
            "number" => FormFieldKind::Number,
            "integer" => FormFieldKind::Integer,
            "boolean" => FormFieldKind::Boolean,
            "multiselect" => FormFieldKind::MultiSelect,
            "external" => FormFieldKind::External,
            _ => return None,
        };
        let options = v
            .get("options")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(|o| {
                        Some(FormOption {
                            value: str_at(o, "/value")?,
                            label: str_at(o, "/label").unwrap_or_default(),
                            description: str_at(o, "/description"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            key: str_at(v, "/key")?,
            title: str_at(v, "/title").unwrap_or_default(),
            description: str_at(v, "/description"),
            kind,
            required: v.get("required").and_then(Value::as_bool).unwrap_or(false),
            options,
            custom: v.get("custom").and_then(Value::as_bool).unwrap_or(false),
            placeholder: str_at(v, "/placeholder"),
            default: v.get("default").cloned().filter(|d| !d.is_null()),
            hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

impl FormRequest {
    pub fn from_json(v: &Value) -> Option<Self> {
        let fields = v
            .get("fields")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(FormField::from_json)
            .collect();
        Some(Self {
            id: str_at(v, "/id")?,
            session_id: str_at(v, "/sessionID")?,
            title: str_at(v, "/title").unwrap_or_default(),
            fields,
        })
    }
}

impl PermissionDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub more: bool,
}

impl FileDiff {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            file: str_at(v, "/file")?,
            patch: str_at(v, "/patch").unwrap_or_default(),
            status: str_at(v, "/status").unwrap_or_default(),
            additions: u64_at(v, "/additions").unwrap_or_default(),
            deletions: u64_at(v, "/deletions").unwrap_or_default(),
            more: v.get("more").and_then(Value::as_bool).unwrap_or(false),
        })
    }
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
                return Some(Block::Tool(Box::new(ToolCall {
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
                    metadata: state
                        .and_then(|s| s.get("metadata"))
                        .cloned()
                        .unwrap_or(Value::Null),
                })));
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
    fn permission_and_diff_parse_server_shapes() {
        let request = PermissionRequest::from_json(&serde_json::json!({
            "id":"perm_a","sessionID":"ses_a","action":"edit","resources":["/tmp/a"],
            "save":["/tmp/*"],"source":{"type":"tool","id":"call_a","messageID":"msg_a"}
        }))
        .unwrap();
        assert_eq!(request.resources, ["/tmp/a"]);
        assert_eq!(request.save, ["/tmp/*"]);
        assert!(PermissionRequest::from_json(&serde_json::json!({"id":"perm_a"})).is_none());
        let diff = FileDiff::from_json(&serde_json::json!({"file":"a.rs","patch":"@@ -1 +1 @@\n-old\n+new", "status":"modified","additions":1,"deletions":1,"more":true})).unwrap();
        assert!(diff.more);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 1);
    }

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
