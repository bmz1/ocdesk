//! Our own normalized event enum. The rest of the app never sees
//! `opencode_codes` types for events, so the protocol crate can be swapped.

#[cfg(test)]
use opencode_codes::StreamEvent;
use opencode_codes::protocol_generated::types::{self as oc, Event as OcEvent};

/// Session run state as reported by `session.status`.
#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Idle,
    Busy,
    Retry { attempt: u64, message: String },
}

#[derive(Clone, Debug)]
#[expect(
    dead_code,
    reason = "payloads are folded into the Store in M1; M0 only logs them"
)]
pub enum Event {
    /// The SSE stream (re)connected. Frames may have been missed: reconcile.
    Connected,
    /// The SSE stream dropped; the pump is backing off and will reconnect.
    Disconnected,
    SessionUpserted(oc::Session),
    SessionDeleted {
        session_id: String,
    },
    SessionStatus {
        session_id: String,
        state: RunState,
    },
    SessionIdle {
        session_id: String,
    },
    SessionError {
        session_id: Option<String>,
        message: String,
    },
    MessageUpserted {
        session_id: String,
        info: oc::Message,
    },
    MessageRemoved {
        session_id: String,
        message_id: String,
    },
    PartUpserted {
        session_id: String,
        part: oc::Part,
    },
    /// Append `delta` to `field` (e.g. "text") of an existing part.
    PartDelta {
        session_id: String,
        message_id: String,
        part_id: String,
        field: String,
        delta: String,
    },
    PartRemoved {
        session_id: String,
        message_id: String,
        part_id: String,
    },
    PermissionAsked(oc::PermissionAskedData),
    PermissionReplied {
        session_id: String,
        request_id: String,
    },
    /// Anything we do not model yet. Carries the wire `type` for logging.
    Other(String),
}

impl Event {
    /// Decode one SSE `data:` payload.
    pub fn from_json(data: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(data).ok()?;
        Some(match serde_json::from_value::<OcEvent>(value.clone()) {
            Ok(ev) => Self::from_oc(ev),
            Err(_) => Event::Other(
                value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?")
                    .to_string(),
            ),
        })
    }

    #[cfg(test)]
    pub fn from_stream(ev: StreamEvent) -> Self {
        match ev {
            StreamEvent::Connected => Event::Connected,
            StreamEvent::Event(ev) => Self::from_oc(*ev),
            StreamEvent::Unknown(v) => Event::Other(
                v.get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?")
                    .to_string(),
            ),
            _ => Event::Other("unmodelled stream item".into()),
        }
    }

    fn from_oc(ev: OcEvent) -> Self {
        match ev {
            OcEvent::SessionCreated(e) => Event::SessionUpserted(e.properties.info),
            OcEvent::SessionUpdated(e) => Event::SessionUpserted(e.properties.info),
            OcEvent::SessionDeleted(e) => Event::SessionDeleted {
                session_id: e.properties.session_id,
            },
            OcEvent::SessionStatus(e) => Event::SessionStatus {
                session_id: e.properties.session_id,
                state: match e.properties.status {
                    oc::SessionStatus::Idle => RunState::Idle,
                    oc::SessionStatus::Busy => RunState::Busy,
                    oc::SessionStatus::Retry {
                        attempt, message, ..
                    } => RunState::Retry { attempt, message },
                },
            },
            OcEvent::SessionIdle(e) => Event::SessionIdle {
                session_id: e.properties.session_id,
            },
            OcEvent::SessionError(e) => Event::SessionError {
                session_id: e.properties.session_id,
                message: e
                    .properties
                    .error
                    .map(|err| crate::store::error_message(&err))
                    .unwrap_or_else(|| "unknown error".into()),
            },
            OcEvent::MessageUpdated(e) => Event::MessageUpserted {
                session_id: e.properties.session_id,
                info: e.properties.info,
            },
            OcEvent::MessageRemoved(e) => Event::MessageRemoved {
                session_id: e.properties.session_id,
                message_id: e.properties.message_id,
            },
            OcEvent::MessagePartUpdated(e) => Event::PartUpserted {
                session_id: e.properties.session_id,
                part: e.properties.part,
            },
            OcEvent::MessagePartDelta(e) => {
                let d = e.properties;
                Event::PartDelta {
                    session_id: d.session_id,
                    message_id: d.message_id,
                    part_id: d.part_id,
                    field: d.field,
                    delta: d.delta,
                }
            }
            OcEvent::MessagePartRemoved(e) => Event::PartRemoved {
                session_id: e.properties.session_id,
                message_id: e.properties.message_id,
                part_id: e.properties.part_id,
            },
            OcEvent::PermissionAsked(e) => Event::PermissionAsked(e.properties),
            OcEvent::PermissionReplied(e) => Event::PermissionReplied {
                session_id: e.properties.session_id,
                request_id: e.properties.request_id,
            },
            other => Event::Other(event_type(&other)),
        }
    }
}

fn event_type(ev: &OcEvent) -> String {
    serde_json::to_value(ev)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| "?".into())
}
