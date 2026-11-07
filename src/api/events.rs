//! Normalized events from opencode 2.x's global stream (`GET /api/event`).
//!
//! Wire envelope: `{id, created, type, data, location?: {directory},
//! durable?: {aggregateID, seq}}`. Durable events carry a per-session `seq`;
//! deltas are live-only and every `*.ended` event repeats the full text, so a
//! client that missed deltas self-heals at the end of each block.

use std::path::PathBuf;

use serde_json::Value;

use super::model::{BlockKind, SessionInfo, content_text, error_text, str_at, u64_at};

#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    pub event: Event,
    /// Durable per-session sequence number, if the event is durable.
    pub seq: Option<u64>,
    pub directory: Option<PathBuf>,
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
    /// The session started working on its inbox.
    ExecutionStarted {
        session_id: String,
    },
    /// The session went idle.
    ExecutionEnded {
        session_id: String,
        outcome: Outcome,
    },
    /// A user prompt entered the session (`session.inbox.enqueued`).
    UserPrompt {
        session_id: String,
        message_id: String,
        text: String,
    },
    StepStarted {
        session_id: String,
        message_id: String,
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
    ToolSucceeded {
        session_id: String,
        message_id: String,
        call_id: String,
        output: String,
    },
    ToolFailed {
        session_id: String,
        message_id: String,
        call_id: String,
        error: String,
    },
    /// `finish` is "stop", "tool-calls", "length", …
    StepEnded {
        session_id: String,
        message_id: String,
        finish: String,
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
    /// Anything we don't model yet. Carries the wire `type` for logging.
    Other(String),
}

impl Envelope {
    pub fn bare(event: Event) -> Self {
        Self {
            event,
            seq: None,
            directory: None,
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
        "session.created" => Event::SessionUpserted(SessionInfo {
            id: sid()?,
            title: String::new(),
            directory: PathBuf::from(d.pointer("/location/directory")?.as_str()?),
            created,
            updated: created,
            parent_id: str_at(d, "/parentID"),
        }),
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
        "session.inbox.enqueued"
            if d.pointer("/item/type").and_then(Value::as_str) == Some("user") =>
        {
            Event::UserPrompt {
                session_id: sid()?,
                message_id: str_at(d, "/inboxID")?,
                text: str_at(d, "/item/payload/text").unwrap_or_default(),
            }
        }
        "session.step.started" => Event::StepStarted {
            session_id: sid()?,
            message_id: mid()?,
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
        "session.tool.success" => Event::ToolSucceeded {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            output: content_text(d.get("content")),
        },
        "session.tool.failed" => Event::ToolFailed {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/id")?,
            error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
        },
        "session.step.ended" => Event::StepEnded {
            session_id: sid()?,
            message_id: mid()?,
            finish: str_at(d, "/finish").unwrap_or_default(),
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
    fn decodes_captured_turns() {
        let evs = fixture();
        let count = |f: fn(&Event) -> bool| evs.iter().filter(|e| f(&e.event)).count();
        assert_eq!(count(|e| matches!(e, Event::UserPrompt { .. })), 2);
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
