//! The event protocol.
//!
//! Every observable thing a session does is an [`Event`]: turn lifecycle,
//! input/output messages, tool execution. Producers build an [`EventRequest`]
//! (no id/sequence — those are assigned by the [`crate::event_log::EventLog`]
//! on append), and observers implement [`EventListener`].
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
    pub const OUTPUT_MESSAGE: &str = "output.message";
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
    OutputMessage {
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
            EventData::OutputMessage { .. } => event_types::OUTPUT_MESSAGE,
            EventData::ToolStarted { .. } => event_types::TOOL_STARTED,
            EventData::ToolCompleted { .. } => event_types::TOOL_COMPLETED,
        }
    }
}

/// A fully-recorded event. `id`, `ts`, and `sequence` are assigned by the
/// event log on append; `sequence` is contiguous per session starting at 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: DateTime<Utc>,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    pub data: EventData,
}

/// An event about to be appended — same shape as [`Event`] minus the fields
/// the log assigns.
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

    /// Finalize into an [`Event`] — called by event log implementations.
    pub fn into_event(self, id: EventId, ts: DateTime<Utc>, sequence: u64) -> Event {
        Event {
            id,
            event_type: self.data.event_type().to_string(),
            ts,
            session_id: self.session_id,
            turn_id: self.turn_id,
            sequence,
            data: self.data,
        }
    }
}

/// Observes events after they are appended to the log. Attach listeners via
/// [`crate::agent::AgentBuilder::listener`].
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
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }
}
