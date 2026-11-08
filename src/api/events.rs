//! Normalized events from opencode 2.x's global stream (`GET /api/event`).
//!
//! Wire envelope: `{id, created, type, data, location?: {directory},
//! durable?: {aggregateID, seq}}`. Durable events carry a per-session `seq`;
//! deltas are live-only and every `*.ended` event repeats the full text, so a
//! client that missed deltas self-heals at the end of each block.

use std::path::PathBuf;

use serde_json::Value;

use super::model::{
    BlockKind, Delivery, FormRequest, InboxItem, ModelRef, PermissionRequest, SessionInfo,
    ShellInfo, content_text, error_text, str_at, u64_at,
};

#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    pub event: Event,
    /// Durable per-session sequence number, if the event is durable.
    pub seq: Option<u64>,
    pub directory: Option<PathBuf>,
    /// Server time of the event (epoch ms); 0 for local events.
    pub created: u64,
}

/// How a turn (an "execution") ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Succeeded,
    /// `reason`: "user", "shutdown", "superseded", "inactivity".
    Interrupted {
        reason: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The stream (re)connected. Live frames may have been missed: reconcile.
    Connected,
    /// The stream dropped; the pump backs off and reconnects.
    Disconnected,
    SessionUpserted(SessionInfo),
    SessionRenamed {
        session_id: String,
        title: String,
    },
    /// Deleted or archived: either way it leaves the list.
    SessionRemoved {
        session_id: String,
    },
    /// `session.model.selected`
    ModelSelected {
        session_id: String,
        model: ModelRef,
    },
    /// `session.agent.selected`
    AgentSelected {
        session_id: String,
        agent: String,
    },
    /// Models, agents or providers changed (config edit, login, …).
    CatalogChanged,
    UsageUpdated {
        session_id: String,
        tokens: Value,
        cost: f64,
    },
    /// The session started working on its inbox.
    ExecutionStarted {
        session_id: String,
    },
    /// The session went idle.
    ExecutionEnded {
        session_id: String,
        outcome: Outcome,
    },
    /// `session.inbox.enqueued`: a prompt (or compaction) is pending.
    InboxEnqueued {
        session_id: String,
        item: InboxItem,
    },
    /// `session.inbox.delivered`: the item reached the transcript (a user
    /// item becomes a user message with the same id).
    InboxDelivered {
        session_id: String,
        inbox_id: String,
    },
    /// `session.inbox.cancelled`
    InboxCancelled {
        session_id: String,
        inbox_id: String,
    },
    /// `session.inbox.delivery.changed`
    InboxDeliveryChanged {
        session_id: String,
        inbox_id: String,
        delivery: Delivery,
    },
    /// `shell.created`
    ShellCreated(ShellInfo),
    /// `shell.exited` (running → exited/timeout/killed)
    ShellExited {
        id: String,
        status: String,
        exit: Option<i64>,
    },
    /// `shell.deleted`
    ShellDeleted {
        id: String,
    },
    /// `vcs.branch.updated` for the envelope's location.
    BranchChanged {
        branch: Option<String>,
    },
    /// `worktree.updated`
    WorktreesChanged {
        project_id: String,
    },
    /// `session.skill.activated`: a skill entry joins the transcript.
    SkillActivated {
        session_id: String,
    },
    /// `command.updated`, `skill.updated`, `mcp.status.changed`: re-read
    /// the folder's `/` menu sources.
    ToolingChanged,
    /// Insert a user message (a delivered inbox item; the demo uses it
    /// directly).
    UserPrompt {
        session_id: String,
        message_id: String,
        text: String,
        attachments: Vec<super::model::Attachment>,
    },
    StepStarted {
        session_id: String,
        message_id: String,
        model: Option<ModelRef>,
        agent: Option<String>,
    },
    BlockStarted {
        session_id: String,
        message_id: String,
        kind: BlockKind,
        ordinal: u64,
    },
    BlockDelta {
        session_id: String,
        message_id: String,
        kind: BlockKind,
        ordinal: u64,
        delta: String,
    },
    BlockEnded {
        session_id: String,
        message_id: String,
        kind: BlockKind,
        ordinal: u64,
        text: String,
    },
    ToolStarted {
        session_id: String,
        message_id: String,
        call_id: String,
        name: String,
    },
    ToolCalled {
        session_id: String,
        message_id: String,
        call_id: String,
        input: Value,
    },
    ToolProgress {
        session_id: String,
        message_id: String,
        call_id: String,
        metadata: Value,
    },
    PermissionAsked(PermissionRequest),
    PermissionReplied {
        session_id: String,
        request_id: String,
    },
    /// A blocking question form was created (`form.created`).
    FormAsked(FormRequest),
    /// A form was answered (`form.replied`); the answer lands as a tool result.
    FormReplied {
        session_id: String,
        form_id: String,
    },
    /// A form was cancelled (`form.cancelled`).
    FormCancelled {
        session_id: String,
        form_id: String,
    },
    ToolSucceeded {
        session_id: String,
        message_id: String,
        call_id: String,
        output: String,
        metadata: Value,
    },
    ToolFailed {
        session_id: String,
        message_id: String,
        call_id: String,
        error: String,
        output: String,
        metadata: Value,
    },
    /// `finish` is "stop", "tool-calls", "length", …
    StepEnded {
        session_id: String,
        message_id: String,
        finish: String,
        tokens: Option<Value>,
        cost: Option<f64>,
    },
    StepFailed {
        session_id: String,
        message_id: String,
        error: String,
    },
    RetryScheduled {
        session_id: String,
        message: String,
    },
    /// `filesystem.changed` for a watched file (see `Api::watch_file`).
    /// `file` is as the server sends it (absolute or folder-relative).
    FileChanged {
        file: String,
        removed: bool,
    },
    /// Anything we don't model yet. Carries the wire `type` for logging.
    Other(String),
}

impl Envelope {
    pub fn bare(event: Event) -> Self {
        Self {
            event,
            seq: None,
            directory: None,
            created: 0,
        }
    }

    /// Decode one SSE `data:` payload.
    pub fn from_json(data: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(data).ok()?;
        let ty = v.get("type")?.as_str()?.to_string();
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        let seq = u64_at(&v, "/durable/seq");
        let directory = v
            .pointer("/location/directory")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let created = u64_at(&v, "/created").unwrap_or(0);
        let event = decode(&ty, &d, created).unwrap_or(Event::Other(ty));
        Some(Self {
            event,
            seq,
            directory,
            created,
        })
    }
}

fn decode(ty: &str, d: &Value, created: u64) -> Option<Event> {
    let sid = || str_at(d, "/sessionID");
    let mid = || str_at(d, "/assistantMessageID");
    let kind = || {
        if ty.starts_with("session.text.") {
            BlockKind::Text
        } else {
            BlockKind::Reasoning
        }
    };
    let ordinal = || u64_at(d, "/ordinal");
    Some(match ty {
        // `session.created` carries the location but no title or times.
        "session.created" => Event::SessionUpserted(
            SessionInfo {
                id: sid()?,
                title: String::new(),
                directory: PathBuf::from(d.pointer("/location/directory")?.as_str()?),
                created,
                updated: created,
                parent_id: str_at(d, "/parentID"),
                model: None,
                // Subagent sessions are created with their description and agent.
                agent: str_at(d, "/agent"),
            }
            .with_title(str_at(d, "/title")),
        ),
        "session.model.selected" => Event::ModelSelected {
            session_id: sid()?,
            model: ModelRef::from_json(d.get("model")?)?,
        },
        "session.agent.selected" => Event::AgentSelected {
            session_id: sid()?,
            agent: str_at(d, "/agent")?,
        },
        // Config changes: the model/agent lists may have changed.
        "model.updated" | "agent.updated" | "provider.updated" => Event::CatalogChanged,
        "session.usage.updated" => Event::UsageUpdated {
            session_id: sid()?,
            tokens: d.get("tokens")?.clone(),
            cost: d.get("cost").and_then(Value::as_f64).unwrap_or(0.),
        },
        "session.renamed" => Event::SessionRenamed {
            session_id: sid()?,
            title: str_at(d, "/title")?,
        },
        "session.deleted" | "session.archived" => Event::SessionRemoved { session_id: sid()? },
        "session.execution.started" => Event::ExecutionStarted { session_id: sid()? },
        "session.execution.succeeded" => Event::ExecutionEnded {
            session_id: sid()?,
            outcome: Outcome::Succeeded,
        },
        "session.execution.interrupted" => Event::ExecutionEnded {
            session_id: sid()?,
            outcome: Outcome::Interrupted {
                reason: str_at(d, "/reason").unwrap_or_default(),
            },
        },
        "session.execution.failed" => Event::ExecutionEnded {
            session_id: sid()?,
            outcome: Outcome::Failed {
                error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
            },
        },
        "session.inbox.enqueued" => {
            let id = str_at(d, "/inboxID")?;
            Event::InboxEnqueued {
                session_id: sid()?,
                item: InboxItem::from_json(Some(&id), d.get("item")?)?,
            }
        }
        "session.inbox.delivered" => Event::InboxDelivered {
            session_id: sid()?,
            inbox_id: str_at(d, "/inboxID")?,
        },
        "session.inbox.cancelled" => Event::InboxCancelled {
            session_id: sid()?,
            inbox_id: str_at(d, "/inboxID")?,
        },
        "session.inbox.delivery.changed" => Event::InboxDeliveryChanged {
            session_id: sid()?,
            inbox_id: str_at(d, "/inboxID")?,
            delivery: Delivery::parse(&str_at(d, "/delivery")?)?,
        },
        "shell.created" => Event::ShellCreated(ShellInfo::from_json(d.get("info")?)?),
        "shell.exited" => Event::ShellExited {
            id: str_at(d, "/id")?,
            status: str_at(d, "/status").unwrap_or_else(|| "exited".into()),
            exit: d.get("exit").and_then(Value::as_f64).map(|e| e as i64),
        },
        "shell.deleted" => Event::ShellDeleted {
            id: str_at(d, "/id")?,
        },
        "filesystem.changed" => Event::FileChanged {
            file: str_at(d, "/file")?,
            removed: d.get("event").and_then(Value::as_str) == Some("unlink"),
        },
        "vcs.branch.updated" => Event::BranchChanged {
            branch: str_at(d, "/branch"),
        },
        "worktree.updated" => Event::WorktreesChanged {
            project_id: str_at(d, "/projectID")?,
        },
        "command.updated" | "skill.updated" | "mcp.status.changed" => Event::ToolingChanged,
        "session.skill.activated" => Event::SkillActivated { session_id: sid()? },
        "session.step.started" => Event::StepStarted {
            session_id: sid()?,
            message_id: mid()?,
            model: d.get("model").and_then(ModelRef::from_json),
            agent: str_at(d, "/agent"),
        },
        "session.text.started" | "session.reasoning.started" => Event::BlockStarted {
            session_id: sid()?,
            message_id: mid()?,
            kind: kind(),
            ordinal: ordinal()?,
        },
        "session.text.delta" | "session.reasoning.delta" => Event::BlockDelta {
            session_id: sid()?,
            message_id: mid()?,
            kind: kind(),
            ordinal: ordinal()?,
            delta: str_at(d, "/delta")?,
        },
        "session.text.ended" | "session.reasoning.ended" => Event::BlockEnded {
            session_id: sid()?,
            message_id: mid()?,
            kind: kind(),
            ordinal: ordinal()?,
            text: str_at(d, "/text").unwrap_or_default(),
        },
        "session.tool.input.started" => Event::ToolStarted {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            name: str_at(d, "/name").unwrap_or_default(),
        },
        "session.tool.called" => Event::ToolCalled {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            input: d.get("input").cloned().unwrap_or(Value::Null),
        },
        "permission.asked" => Event::PermissionAsked(PermissionRequest::from_json(d)?),
        "permission.replied" => Event::PermissionReplied {
            session_id: sid()?,
            request_id: str_at(d, "/requestID")?,
        },
        "form.created" => Event::FormAsked(FormRequest::from_json(d.get("form")?)?),
        "form.replied" => Event::FormReplied {
            session_id: sid()?,
            form_id: str_at(d, "/id")?,
        },
        "form.cancelled" => Event::FormCancelled {
            session_id: sid()?,
            form_id: str_at(d, "/id")?,
        },
        "session.tool.progress" => Event::ToolProgress {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            metadata: d.get("metadata").cloned().unwrap_or(Value::Null),
        },
        "session.tool.success" => Event::ToolSucceeded {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            output: content_text(d.get("content")),
            metadata: d.get("metadata").cloned().unwrap_or(Value::Null),
        },
        "session.tool.failed" => Event::ToolFailed {
            output: content_text(d.get("content")),
            metadata: d.get("metadata").cloned().unwrap_or(Value::Null),
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
        },
        "session.step.ended" => Event::StepEnded {
            session_id: sid()?,
            message_id: mid()?,
            finish: str_at(d, "/finish").unwrap_or_default(),
            tokens: d.get("tokens").cloned(),
            cost: d.get("cost").and_then(Value::as_f64),
        },
        "session.step.failed" => Event::StepFailed {
            session_id: sid()?,
            message_id: mid()?,
            error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
        },
        "session.retry.scheduled" => Event::RetryScheduled {
            session_id: sid()?,
            message: error_text(d.get("error"))
                .or_else(|| str_at(d, "/message"))
                .unwrap_or_default(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Envelope> {
        include_str!("../../tests/fixtures/v2-events.jsonl")
            .lines()
            .map(|l| Envelope::from_json(l).expect("every captured frame decodes"))
            .collect()
    }

    #[test]
    fn decodes_model_and_agent_selection() {
        // Shapes captured from opencode 2.2.6.
        let m = Envelope::from_json(r#"{"type":"session.model.selected","data":{"sessionID":"ses_a","model":{"id":"mock-2","providerID":"mock"}},"durable":{"aggregateID":"ses_a","seq":1,"version":1}}"#).unwrap();
        assert_eq!(
            m.event,
            Event::ModelSelected {
                session_id: "ses_a".into(),
                model: ModelRef {
                    provider_id: "mock".into(),
                    id: "mock-2".into(),
                    variant: None
                },
            }
        );
        let a = Envelope::from_json(
            r#"{"type":"session.agent.selected","data":{"sessionID":"ses_a","agent":"plan"}}"#,
        )
        .unwrap();
        assert_eq!(
            a.event,
            Event::AgentSelected {
                session_id: "ses_a".into(),
                agent: "plan".into()
            }
        );
        let c = Envelope::from_json(r#"{"type":"agent.updated","data":{}}"#).unwrap();
        assert_eq!(c.event, Event::CatalogChanged);
    }

    #[test]
    fn decodes_filesystem_changed() {
        // Shape from the 2.2.6 OpenAPI schema (`filesystem.changed`).
        let e = Envelope::from_json(r#"{"id":"evt_1","created":5,"type":"filesystem.changed","location":{"directory":"/repo"},"data":{"file":"src/a.rs","event":"unlink"}}"#).unwrap();
        assert_eq!(
            e.event,
            Event::FileChanged {
                file: "src/a.rs".into(),
                removed: true
            }
        );
        assert_eq!(e.directory.as_deref(), Some(std::path::Path::new("/repo")));
        let c = Envelope::from_json(
            r#"{"type":"filesystem.changed","data":{"file":"/repo/b.rs","event":"change"}}"#,
        )
        .unwrap();
        assert!(matches!(c.event, Event::FileChanged { removed: false, .. }));
    }

    #[test]
    fn decodes_inbox_shell_vcs_events() {
        // Shapes captured from opencode 2.2.6 (docs/protocol-notes.md, M7).
        let ev = |s: &str| Envelope::from_json(s).unwrap().event;
        assert!(matches!(
            ev(r#"{"type":"session.inbox.enqueued","data":{"inboxID":"msg_q","sessionID":"ses_a","item":{"type":"user","payload":{"text":"later"},"delivery":"queue"}}}"#),
            Event::InboxEnqueued { item, .. } if item.id == "msg_q" && item.text == "later" && item.delivery == Delivery::Queue
        ));
        assert!(matches!(
            ev(
                r#"{"type":"session.inbox.delivery.changed","data":{"sessionID":"ses_a","inboxID":"msg_q","delivery":"steer"}}"#
            ),
            Event::InboxDeliveryChanged {
                delivery: Delivery::Steer,
                ..
            }
        ));
        assert!(matches!(
            ev(r#"{"type":"session.inbox.cancelled","data":{"sessionID":"ses_a","inboxID":"msg_q"}}"#),
            Event::InboxCancelled { inbox_id, .. } if inbox_id == "msg_q"
        ));
        assert!(matches!(
            ev(r#"{"type":"shell.exited","data":{"id":"sh_1","status":"killed"}}"#),
            Event::ShellExited { status, exit: None, .. } if status == "killed"
        ));
        let created = Envelope::from_json(r#"{"type":"vcs.branch.updated","location":{"directory":"/p"},"data":{"branch":"main"}}"#).unwrap();
        assert_eq!(
            created.directory.as_deref(),
            Some(std::path::Path::new("/p"))
        );
        assert!(matches!(created.event, Event::BranchChanged { branch: Some(b) } if b == "main"));
        // A subagent session arrives with its parent, title and agent.
        assert!(matches!(
            ev(r#"{"type":"session.created","created":5,"data":{"sessionID":"ses_c","parentID":"ses_p","title":"Look around","agent":"general","location":{"directory":"/p"}}}"#),
            Event::SessionUpserted(s) if s.parent_id.as_deref() == Some("ses_p") && s.title == "Look around" && s.agent.as_deref() == Some("general")
        ));
    }

    #[test]
    fn decodes_form_events() {
        // Shape captured from a live sidecar (opencode 2.2.6 question tool).
        let created = Envelope::from_json(concat!(
            r#"{"type":"form.created","data":{"form":{"id":"frm_1","sessionID":"ses_a","title":"Questions","#,
            r#""metadata":{"kind":"question"},"fields":["#,
            r#"{"key":"q0","title":"404 banner","type":"string","description":"Still visible?","#,
            r#""options":[{"value":"Yes, still there","label":"Yes, still there"}],"custom":true},"#,
            r#"{"key":"q1","title":"When","type":"string","options":[{"value":"Right at startup","label":"Right at startup"}]}"#,
            r#"]}}}"#,
        ))
        .unwrap();
        assert!(matches!(
            created.event,
            Event::FormAsked(f) if f.id == "frm_1" && f.fields.len() == 2 && f.fields[0].custom
        ));
        let replied = Envelope::from_json(
            r#"{"type":"form.replied","data":{"sessionID":"ses_a","id":"frm_1"}}"#,
        )
        .unwrap();
        assert!(
            matches!(replied.event, Event::FormReplied { session_id, form_id } if form_id == "frm_1" && session_id == "ses_a")
        );
        let cancelled = Envelope::from_json(
            r#"{"type":"form.cancelled","data":{"sessionID":"ses_a","id":"frm_1"}}"#,
        )
        .unwrap();
        assert!(matches!(
            cancelled.event,
            Event::FormCancelled { form_id, .. } if form_id == "frm_1"
        ));
    }

    #[test]
    fn decodes_permissions_and_progress() {
        let asked = Envelope::from_json(r#"{"type":"permission.asked","data":{"id":"perm_a","sessionID":"ses_a","action":"shell","resources":["ls"]}}"#).unwrap();
        assert!(
            matches!(asked.event, Event::PermissionAsked(r) if r.id == "perm_a" && r.session_id == "ses_a")
        );
        let replied = Envelope::from_json(r#"{"type":"permission.replied","data":{"sessionID":"ses_a","requestID":"perm_a","reply":"once"}}"#).unwrap();
        assert!(
            matches!(replied.event, Event::PermissionReplied { request_id, .. } if request_id == "perm_a")
        );
        let progress = Envelope::from_json(r#"{"type":"session.tool.progress","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","id":"call_a","metadata":{"diff":"+hello"}}}"#).unwrap();
        assert!(
            matches!(progress.event, Event::ToolProgress { metadata, .. } if metadata["diff"] == "+hello")
        );
    }

    #[test]
    fn decodes_captured_turns() {
        let evs = fixture();
        let count = |f: fn(&Event) -> bool| evs.iter().filter(|e| f(&e.event)).count();
        assert_eq!(
            count(|e| matches!(e, Event::InboxEnqueued { item, .. } if item.kind == "user")),
            2
        );
        assert_eq!(count(|e| matches!(e, Event::InboxDelivered { .. })), 2);
        assert_eq!(count(|e| matches!(e, Event::ExecutionStarted { .. })), 2);
        assert_eq!(
            count(|e| matches!(
                e,
                Event::ExecutionEnded {
                    outcome: Outcome::Succeeded,
                    ..
                }
            )),
            1
        );
        assert_eq!(
            count(
                |e| matches!(e, Event::ExecutionEnded { outcome: Outcome::Interrupted { reason }, .. } if reason == "user")
            ),
            1
        );
        assert_eq!(
            count(
                |e| matches!(e, Event::ToolSucceeded { output, .. } if output.contains("hello-from-bash"))
            ),
            1
        );
        assert_eq!(count(|e| matches!(e, Event::SessionRenamed { .. })), 1);
        assert!(
            count(|e| matches!(
                e,
                Event::BlockDelta {
                    kind: BlockKind::Text,
                    ..
                }
            )) > 10
        );
        let created = evs
            .iter()
            .find(|e| matches!(e.event, Event::SessionUpserted(_)))
            .unwrap();
        assert_eq!(created.seq, Some(0));
        assert!(created.directory.is_some());
    }
}
