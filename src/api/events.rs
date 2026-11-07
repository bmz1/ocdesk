//! Normalized events from opencode's v2 global stream (`GET /api/event`).
//!
//! Wire envelope: `{id, type, data, location?: {directory}, durable?: {aggregateID, seq}}`.
//! Durable events carry a per-session `seq`; deltas are live-only and every
//! `*.ended` event repeats the full text, so a client that missed deltas
//! self-heals at the end of each block.

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

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The stream (re)connected. Live frames may have been missed: reconcile.
    Connected,
    /// The stream dropped; the pump backs off and reconnects.
    Disconnected,
    SessionUpserted(SessionInfo),
    SessionDeleted {
        session_id: String,
    },
    /// A user prompt was delivered into the transcript.
    Prompted {
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
        block_id: String,
    },
    BlockDelta {
        session_id: String,
        message_id: String,
        kind: BlockKind,
        block_id: String,
        delta: String,
    },
    BlockEnded {
        session_id: String,
        message_id: String,
        kind: BlockKind,
        block_id: String,
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
        name: String,
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
    /// `finish` is "stop", "tool-calls", "length", … ("tool-calls" means
    /// another step follows).
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
    Retried {
        session_id: String,
        attempt: u64,
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
        let event = decode(&ty, &d).unwrap_or(Event::Other(ty));
        Some(Self {
            event,
            seq,
            directory,
        })
    }
}

fn decode(ty: &str, d: &Value) -> Option<Event> {
    let sid = || str_at(d, "/sessionID");
    let mid = || str_at(d, "/assistantMessageID");
    let block = |kind: BlockKind| -> Option<(String, String, BlockKind, String)> {
        let id_field = if kind == BlockKind::Text {
            "/textID"
        } else {
            "/reasoningID"
        };
        Some((sid()?, mid()?, kind, str_at(d, id_field)?))
    };
    let kind_of = |ty: &str| {
        if ty.starts_with("session.next.text.") {
            BlockKind::Text
        } else {
            BlockKind::Reasoning
        }
    };
    Some(match ty {
        "session.created" | "session.updated" => {
            Event::SessionUpserted(SessionInfo::from_json(d.get("info")?)?)
        }
        "session.deleted" => Event::SessionDeleted {
            session_id: sid().or_else(|| str_at(d, "/info/id"))?,
        },
        "session.next.prompted" => Event::Prompted {
            session_id: sid()?,
            message_id: str_at(d, "/messageID")?,
            text: str_at(d, "/prompt/text").unwrap_or_default(),
        },
        "session.next.step.started" => Event::StepStarted {
            session_id: sid()?,
            message_id: mid()?,
        },
        "session.next.text.started" | "session.next.reasoning.started" => {
            let (session_id, message_id, kind, block_id) = block(kind_of(ty))?;
            Event::BlockStarted {
                session_id,
                message_id,
                kind,
                block_id,
            }
        }
        "session.next.text.delta" | "session.next.reasoning.delta" => {
            let (session_id, message_id, kind, block_id) = block(kind_of(ty))?;
            Event::BlockDelta {
                session_id,
                message_id,
                kind,
                block_id,
                delta: str_at(d, "/delta")?,
            }
        }
        "session.next.text.ended" | "session.next.reasoning.ended" => {
            let (session_id, message_id, kind, block_id) = block(kind_of(ty))?;
            Event::BlockEnded {
                session_id,
                message_id,
                kind,
                block_id,
                text: str_at(d, "/text").unwrap_or_default(),
            }
        }
        "session.next.tool.input.started" => Event::ToolStarted {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/callID")?,
            name: str_at(d, "/name").unwrap_or_default(),
        },
        "session.next.tool.called" => Event::ToolCalled {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/callID")?,
            name: str_at(d, "/tool").unwrap_or_default(),
            input: d.get("input").cloned().unwrap_or(Value::Null),
        },
        "session.next.tool.success" => Event::ToolSucceeded {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/callID")?,
            output: content_text(d.get("content")),
        },
        "session.next.tool.failed" => Event::ToolFailed {
            session_id: sid()?,
            message_id: mid()?,
            call_id: str_at(d, "/callID")?,
            error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
        },
        "session.next.step.ended" => Event::StepEnded {
            session_id: sid()?,
            message_id: mid()?,
            finish: str_at(d, "/finish").unwrap_or_default(),
        },
        "session.next.step.failed" => Event::StepFailed {
            session_id: sid()?,
            message_id: mid()?,
            error: error_text(d.get("error")).unwrap_or_else(|| "failed".into()),
        },
        "session.next.retried" => Event::Retried {
            session_id: sid()?,
            attempt: u64_at(d, "/attempt").unwrap_or(0),
            message: error_text(d.get("error")).unwrap_or_default(),
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
    fn decodes_captured_v2_turn() {
        let evs = fixture();
        let count = |f: fn(&Event) -> bool| evs.iter().filter(|e| f(&e.event)).count();
        assert_eq!(count(|e| matches!(e, Event::Prompted { .. })), 2);
        assert_eq!(
            count(|e| matches!(e, Event::ToolCalled { name, .. } if name == "bash")),
            1
        );
        assert_eq!(
            count(
                |e| matches!(e, Event::ToolSucceeded { output, .. } if output.contains("hello-from-bash"))
            ),
            1
        );
        assert_eq!(
            count(|e| matches!(e, Event::StepEnded { finish, .. } if finish == "tool-calls")),
            1
        );
        assert_eq!(
            count(
                |e| matches!(e, Event::StepFailed { error, .. } if error == "Provider turn interrupted")
            ),
            1
        );
        assert!(
            count(|e| matches!(
                e,
                Event::BlockDelta {
                    kind: BlockKind::Text,
                    ..
                }
            )) > 10
        );
        // Durable events carry seq and location.
        let created = evs
            .iter()
            .find(|e| matches!(e.event, Event::SessionUpserted(_)))
            .unwrap();
        assert_eq!(created.seq, Some(0));
        assert_eq!(
            created.directory.as_deref(),
            Some(std::path::Path::new("/tmp/claude-0/v2proj"))
        );
    }
}
