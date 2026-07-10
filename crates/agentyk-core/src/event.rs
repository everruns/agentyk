//! The event protocol.
//!
//! Every observable thing a session does is an [`Event`]: turn lifecycle,
//! input/output messages, tool execution. Producers build an [`EventRequest`]
//! (no id/sequence — those are assigned on append), and observers implement
//! [`EventListener`].
//!
//! Most event types are **durable**: appended to the
//! [`crate::event_log::EventLog`] and assigned a sequence. A few are
//! **ephemeral** ([`EventData::is_ephemeral`]) — streaming deltas, delivered
//! to listeners in real time but never persisted or sequenced
//! (`sequence: None`). [`crate::executor::TurnHost::record`] is what branches
//! on this; the event log itself only ever sees durable events.
//!
//! Event types use the everruns dot notation (`turn.started`,
//! `input.message`, `tool.completed`, …) so the protocol stays recognizable
//! when everruns-core is rebuilt on top of this crate.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{EventId, SessionId, TurnId};
use crate::message::{Message, ToolCall};

/// Dot-notation event type strings.
pub mod event_types {
    pub const TURN_STARTED: &str = "turn.started";
    pub const TURN_COMPLETED: &str = "turn.completed";
    pub const TURN_FAILED: &str = "turn.failed";
    pub const INPUT_MESSAGE: &str = "input.message";
    pub const OUTPUT_MESSAGE_STARTED: &str = "output.message.started";
    pub const OUTPUT_MESSAGE_DELTA: &str = "output.message.delta";
    pub const OUTPUT_MESSAGE_COMPLETED: &str = "output.message.completed";
    pub const TOOL_STARTED: &str = "tool.started";
    pub const TOOL_COMPLETED: &str = "tool.completed";
}

/// Typed event payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventData {
    TurnStarted,
    TurnCompleted {
        iterations: usize,
        tool_calls: usize,
    },
    TurnFailed {
        error: String,
    },
    InputMessage {
        message: Message,
    },
    /// The model started generating a response for this reason step. UIs can
    /// show a "thinking" indicator until a delta or the completed event
    /// arrives.
    OutputMessageStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// 1-based iteration within the turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iteration: Option<u32>,
    },
    /// Incremental text update during generation. **Ephemeral** — delivered
    /// to listeners, never persisted.
    OutputMessageDelta {
        delta: String,
        accumulated: String,
    },
    /// The reason step finished; `message` is the fully materialized
    /// assistant message (text and/or tool calls).
    OutputMessageCompleted {
        message: Message,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolCompleted {
        call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
}

impl EventData {
    /// The dot-notation type string for this payload.
    pub fn event_type(&self) -> &'static str {
        match self {
            EventData::TurnStarted => event_types::TURN_STARTED,
            EventData::TurnCompleted { .. } => event_types::TURN_COMPLETED,
            EventData::TurnFailed { .. } => event_types::TURN_FAILED,
            EventData::InputMessage { .. } => event_types::INPUT_MESSAGE,
            EventData::OutputMessageStarted { .. } => event_types::OUTPUT_MESSAGE_STARTED,
            EventData::OutputMessageDelta { .. } => event_types::OUTPUT_MESSAGE_DELTA,
            EventData::OutputMessageCompleted { .. } => event_types::OUTPUT_MESSAGE_COMPLETED,
            EventData::ToolStarted { .. } => event_types::TOOL_STARTED,
            EventData::ToolCompleted { .. } => event_types::TOOL_COMPLETED,
        }
    }

    /// Ephemeral events are delivered to listeners but never persisted —
    /// see the module docs. Only `output.message.delta` is ephemeral today.
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, EventData::OutputMessageDelta { .. })
    }
}

/// A fully-recorded event. `id` and `ts` are always assigned on emission;
/// `sequence` is `Some` (contiguous per session, starting at 1) for durable
/// events and `None` for ephemeral ones — see [`EventData::is_ephemeral`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: DateTime<Utc>,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub data: EventData,
}

/// An event about to be emitted — same shape as [`Event`] minus the fields
/// assigned on emission.
#[derive(Debug, Clone)]
pub struct EventRequest {
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub data: EventData,
}

impl EventRequest {
    pub fn new(session_id: SessionId, data: EventData) -> Self {
        Self {
            session_id,
            turn_id: None,
            data,
        }
    }

    pub fn with_turn(session_id: SessionId, turn_id: TurnId, data: EventData) -> Self {
        Self {
            session_id,
            turn_id: Some(turn_id),
            data,
        }
    }

    /// Finalize into a durable [`Event`] with an assigned sequence — called
    /// by [`crate::event_log::EventLog`] implementations.
    pub fn into_event(self, id: EventId, ts: DateTime<Utc>, sequence: u64) -> Event {
        Event {
            id,
            event_type: self.data.event_type().to_string(),
            ts,
            session_id: self.session_id,
            turn_id: self.turn_id,
            sequence: Some(sequence),
            data: self.data,
        }
    }

    /// Finalize into an ephemeral [`Event`] (`sequence: None`) without going
    /// through an [`crate::event_log::EventLog`] — called by
    /// [`crate::executor::TurnHost::record`] for events where
    /// [`EventData::is_ephemeral`] is true.
    pub fn into_ephemeral_event(self, id: EventId, ts: DateTime<Utc>) -> Event {
        Event {
            id,
            event_type: self.data.event_type().to_string(),
            ts,
            session_id: self.session_id,
            turn_id: self.turn_id,
            sequence: None,
            data: self.data,
        }
    }
}

/// Observes events after they are emitted. Attach listeners via
/// `AgentBuilder::listener`. Receives both durable and ephemeral events —
/// check [`Event::sequence`] (`None` for ephemeral) to distinguish.
#[async_trait]
pub trait EventListener: Send + Sync {
    async fn on_event(&self, event: &Event);

    /// Human-readable name for diagnostics.
    fn name(&self) -> &'static str {
        "listener"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serde_roundtrip() {
        let request = EventRequest::new(
            SessionId::new(),
            EventData::InputMessage {
                message: Message::user("hi"),
            },
        );
        let event = request.into_event(EventId::new(), Utc::now(), 1);
        assert_eq!(event.event_type, "input.message");
        assert_eq!(event.sequence, Some(1));
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn ephemeral_event_has_no_sequence() {
        let request = EventRequest::new(
            SessionId::new(),
            EventData::OutputMessageDelta {
                delta: "hi".into(),
                accumulated: "hi".into(),
            },
        );
        assert!(request.data.is_ephemeral());
        let event = request.into_ephemeral_event(EventId::new(), Utc::now());
        assert_eq!(event.sequence, None);
    }

    #[test]
    fn only_delta_is_ephemeral() {
        assert!(!EventData::TurnStarted.is_ephemeral());
        assert!(
            !EventData::OutputMessageStarted {
                model: None,
                iteration: None
            }
            .is_ephemeral()
        );
        assert!(
            !EventData::OutputMessageCompleted {
                message: Message::assistant("hi"),
            }
            .is_ephemeral()
        );
    }
}
