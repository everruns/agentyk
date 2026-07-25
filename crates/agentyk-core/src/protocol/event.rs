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

use crate::id::{EventId, MessageId, SessionId, TurnId};
use crate::message::{Message, ToolCall};

/// The dot-notation type string of every event this crate emits, as it
/// appears in [`Event::event_type`] and on disk.
///
/// Match on these rather than on string literals: a typo in a literal is a
/// filter that silently never fires.
pub mod event_types {
    /// A turn began; the input message follows.
    pub const TURN_STARTED: &str = "turn.started";
    /// A turn ended with a final assistant answer.
    pub const TURN_COMPLETED: &str = "turn.completed";
    /// A turn ended in failure — a driver error, or the iteration ceiling.
    pub const TURN_FAILED: &str = "turn.failed";
    /// A turn was stopped by its caller.
    pub const TURN_CANCELLED: &str = "turn.cancelled";
    /// A turn was stopped deliberately by policy — see
    /// [`crate::turn::SealReason`].
    pub const TURN_SEALED: &str = "turn.sealed";
    /// The user message that opened the turn.
    pub const INPUT_MESSAGE: &str = "input.message";
    /// The model began generating an assistant message.
    pub const OUTPUT_MESSAGE_STARTED: &str = "output.message.started";
    /// An incremental text update. Ephemeral: never persisted.
    pub const OUTPUT_MESSAGE_DELTA: &str = "output.message.delta";
    /// The assistant message is complete, text and tool calls included.
    pub const OUTPUT_MESSAGE_COMPLETED: &str = "output.message.completed";
    /// A tool call is about to run, with the arguments it will run on.
    pub const TOOL_STARTED: &str = "tool.started";
    /// A tool call resolved, successfully or not.
    pub const TOOL_COMPLETED: &str = "tool.completed";
    /// Middleware blocked a tool call before it ran.
    pub const TOOL_DENIED: &str = "tool.denied";
    /// Middleware rewrote a tool call before it ran.
    pub const TOOL_REWRITTEN: &str = "tool.rewritten";
}

/// The payload of an [`Event`] — what actually happened.
///
/// `#[non_exhaustive]`: new event types are added as the protocol grows, and
/// a log written by a newer agentyk stays readable by an older one (unknown
/// kinds degrade to [`EventData::Custom`], see [`Event`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventData {
    /// A turn began. The `input.message` event follows immediately.
    TurnStarted,
    /// A turn ended with a final assistant answer.
    TurnCompleted {
        /// LLM completions performed within the turn.
        iterations: usize,
        /// Tool calls executed within the turn.
        tool_calls: usize,
    },
    /// A turn ended in failure. Also covers hitting the iteration ceiling,
    /// where no answer was ever produced.
    TurnFailed {
        /// Human-readable description of what went wrong.
        error: String,
    },
    /// The turn was stopped via a [`crate::cancellation::CancellationToken`]
    /// rather than failing or completing.
    TurnCancelled,
    /// The turn was deliberately sealed — see
    /// [`crate::turn::SealReason`].
    TurnSealed {
        /// Why the turn was sealed rather than allowed to continue.
        reason: crate::turn::SealReason,
    },
    /// The user message that opened the turn.
    InputMessage {
        /// The input, as it enters history.
        message: Message,
    },
    /// The model started generating a response for this reason step. UIs can
    /// show a "thinking" indicator until a delta or the completed event
    /// arrives. `message_id` correlates this event with the deltas and the
    /// completed event of the same logical assistant message (mirrors
    /// everruns' streaming lifecycle correlation).
    OutputMessageStarted {
        /// Stable id shared by the started/delta/completed events of one
        /// assistant message. `#[serde(default)]` so pre-0.1.1 logs (which
        /// lack it) still deserialize, gaining a fresh id.
        #[serde(default)]
        message_id: MessageId,
        /// The wire model name this step ran on, when the executor knows it —
        /// which may differ per turn, via
        /// [`crate::controls::TurnControls`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// 1-based iteration within the turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iteration: Option<u32>,
    },
    /// Incremental text update during generation. **Ephemeral** — delivered
    /// to listeners, never persisted. Carries the same `message_id` as its
    /// started/completed siblings.
    OutputMessageDelta {
        /// Ties this delta to its `started`/`completed` siblings.
        #[serde(default)]
        message_id: MessageId,
        /// Just-generated text, to append.
        delta: String,
        /// All text generated so far, `delta` included — for consumers that
        /// would rather replace than append.
        accumulated: String,
    },
    /// The reason step finished; `message` is the fully materialized
    /// assistant message (text and/or tool calls). Same `message_id` as the
    /// started event.
    OutputMessageCompleted {
        /// Same id as the `started` event and every delta between them.
        #[serde(default)]
        message_id: MessageId,
        /// The finished assistant message — this is what enters history.
        message: Message,
    },
    /// A tool call is about to run. The call carried here is the one that
    /// will actually execute, so it reflects any middleware rewrite.
    ToolStarted {
        /// The call as it will run: id, name, and arguments.
        call: ToolCall,
    },
    /// A tool call resolved. Recorded for denied calls too, carrying the
    /// denial reason as an error result, because that is what the model sees.
    ToolCompleted {
        /// Correlates with the `tool.started` event for this call.
        call_id: String,
        /// The tool that ran.
        name: String,
        /// What the model receives as the result.
        output: String,
        /// Whether `output` describes a failure. Tool failures are results,
        /// not turn failures: the model gets to react to them.
        is_error: bool,
    },
    /// A [`crate::middleware::TurnMiddleware`] denied this call before it ran.
    /// Recorded alongside (not instead of) the usual
    /// `tool.started`/`tool.completed` pair — the denial reason also
    /// becomes the (error) `tool.completed` output the model sees.
    ToolDenied {
        /// The call that was blocked.
        call_id: String,
        /// The tool that would have run.
        name: String,
        /// Why it was blocked. Also becomes the error result the model sees.
        reason: String,
    },
    /// A [`crate::middleware::TurnMiddleware`] rewrote this call before it
    /// ran — argument redaction, path rewriting. Recorded so a mutation is
    /// never invisible: `name` is the call as executed, and `tool.started`
    /// carries the executed call. The everruns analogue of
    /// `output.message.replaced` for the act phase.
    ToolRewritten {
        /// The call that was rewritten. Unchanged by the rewrite — the
        /// pending batch is keyed by it.
        call_id: String,
        /// The tool name as executed, which a rewrite may have changed.
        name: String,
        /// Which middleware rewrote it, from
        /// [`crate::middleware::TurnMiddleware::name`], when the executor
        /// knows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// Escape hatch for domain events this crate doesn't know about yet —
    /// a capability or host emitting its own event without forking core.
    /// `event_type` should follow the dot-notation convention (e.g.
    /// `"budget.warning"`); as protocol coverage grows, well-known custom
    /// types are expected to graduate into first-class variants (see
    /// `specs/everruns-adoption.md`).
    Custom {
        /// Dot-notation type, e.g. `"budget.warning"`. Becomes
        /// [`Event::event_type`].
        event_type: String,
        /// Whatever the emitter wants to carry. Core never interprets it.
        #[serde(default)]
        payload: serde_json::Value,
    },
}

impl EventData {
    /// Build a [`EventData::Custom`] event — the escape hatch for domain
    /// events core has no variant for.
    pub fn custom(event_type: impl Into<String>, payload: serde_json::Value) -> Self {
        EventData::Custom {
            event_type: event_type.into(),
            payload,
        }
    }

    /// The dot-notation type string for this payload.
    pub fn event_type(&self) -> &str {
        match self {
            EventData::TurnStarted => event_types::TURN_STARTED,
            EventData::TurnCompleted { .. } => event_types::TURN_COMPLETED,
            EventData::TurnFailed { .. } => event_types::TURN_FAILED,
            EventData::TurnCancelled => event_types::TURN_CANCELLED,
            EventData::TurnSealed { .. } => event_types::TURN_SEALED,
            EventData::InputMessage { .. } => event_types::INPUT_MESSAGE,
            EventData::OutputMessageStarted { .. } => event_types::OUTPUT_MESSAGE_STARTED,
            EventData::OutputMessageDelta { .. } => event_types::OUTPUT_MESSAGE_DELTA,
            EventData::OutputMessageCompleted { .. } => event_types::OUTPUT_MESSAGE_COMPLETED,
            EventData::ToolStarted { .. } => event_types::TOOL_STARTED,
            EventData::ToolCompleted { .. } => event_types::TOOL_COMPLETED,
            EventData::ToolDenied { .. } => event_types::TOOL_DENIED,
            EventData::ToolRewritten { .. } => event_types::TOOL_REWRITTEN,
            EventData::Custom { event_type, .. } => event_type,
        }
    }

    /// Ephemeral events are delivered to listeners but never persisted —
    /// see the module docs. Only `output.message.delta` is ephemeral today;
    /// `Custom` events are durable by default (a custom *ephemeral* event
    /// isn't representable yet — add one if a use case needs it).
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, EventData::OutputMessageDelta { .. })
    }
}

/// A fully-recorded event. `id` and `ts` are always assigned on emission;
/// `sequence` is `Some` (contiguous per session, starting at 1) for durable
/// events and `None` for ephemeral ones — see [`EventData::is_ephemeral`].
///
/// Deserialization is **forward compatible**: an event whose `kind` this
/// version doesn't know degrades to [`EventData::Custom`] carrying the
/// original payload, rather than failing the read — see the custom
/// [`Deserialize`] impl below.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Event {
    /// Unique id, assigned on emission.
    pub id: EventId,
    /// Dot-notation type — see [`event_types`]. Denormalized from `data` so
    /// consumers can filter without matching the payload.
    #[serde(rename = "type")]
    pub event_type: String,
    /// When it was emitted.
    pub ts: DateTime<Utc>,
    /// The session it belongs to.
    pub session_id: SessionId,
    /// The turn it belongs to, for events emitted inside one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Position in the session's durable order, starting at 1. `None` marks
    /// an ephemeral event, which was delivered to listeners but never
    /// persisted — see [`EventData::is_ephemeral`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    /// What happened.
    pub data: EventData,
}

/// Mirrors [`Event`] with the payload left undecoded, so an unknown `kind`
/// can be caught and degraded instead of failing the whole read.
#[derive(Deserialize)]
struct EventRepr {
    id: EventId,
    #[serde(rename = "type")]
    event_type: String,
    ts: DateTime<Utc>,
    session_id: SessionId,
    #[serde(default)]
    turn_id: Option<TurnId>,
    #[serde(default)]
    sequence: Option<u64>,
    data: serde_json::Value,
}

// Replay is the persistence contract: a log must stay readable, and an older
// binary must not choke on a log a newer one wrote. `EventData` is internally
// tagged, so an unrecognized `kind` would otherwise abort deserialization of
// the whole line — and `JsonlEventLog::read` turns that into a failed read of
// the entire session. Degrading to `Custom` keeps the event type and its full
// payload, which is exactly what that variant exists for; nothing is lost but
// the static type.
//
// This intentionally routes through `serde_json::Value`: the event log is a
// JSON format by construction, and a self-describing intermediate is what
// makes the fallback possible at all.
impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = EventRepr::deserialize(deserializer)?;
        let data =
            serde_json::from_value::<EventData>(repr.data.clone()).unwrap_or(EventData::Custom {
                event_type: repr.event_type.clone(),
                payload: repr.data,
            });
        Ok(Event {
            id: repr.id,
            event_type: repr.event_type,
            ts: repr.ts,
            session_id: repr.session_id,
            turn_id: repr.turn_id,
            sequence: repr.sequence,
            data,
        })
    }
}

/// An event about to be emitted — same shape as [`Event`] minus the fields
/// assigned on emission.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EventRequest {
    /// The session this event belongs to.
    pub session_id: SessionId,
    /// The turn it belongs to, if it was emitted inside one.
    pub turn_id: Option<TurnId>,
    /// What happened.
    pub data: EventData,
}

impl EventRequest {
    /// An event not tied to any particular turn.
    pub fn new(session_id: SessionId, data: EventData) -> Self {
        Self {
            session_id,
            turn_id: None,
            data,
        }
    }

    /// An event emitted during a turn — what an executor records.
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
    /// Called for every event, durable and ephemeral, in emission order.
    ///
    /// Listeners run inline on the turn's write path, so slow work here slows
    /// the turn; hand it to a channel or task instead. A listener cannot
    /// change the event or stop the turn — that is what middleware is for.
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
                message_id: MessageId::new(),
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
                message_id: MessageId::new(),
                model: None,
                iteration: None
            }
            .is_ephemeral()
        );
        assert!(
            !EventData::OutputMessageCompleted {
                message_id: MessageId::new(),
                message: Message::assistant("hi"),
            }
            .is_ephemeral()
        );
    }

    #[test]
    fn an_unknown_event_kind_degrades_to_custom_instead_of_failing() {
        // A line written by a future version: `turn.compacted` is not a kind
        // this build knows. Reading it must not fail — replay is the
        // persistence contract, and one unreadable line fails a whole session
        // read in `JsonlEventLog`.
        let line = serde_json::json!({
            "id": EventId::new(),
            "type": "turn.compacted",
            "ts": Utc::now(),
            "session_id": SessionId::new(),
            "sequence": 7,
            "data": {"kind": "turn_compacted", "removed": 12, "note": "from a newer agentyk"},
        })
        .to_string();

        let event: Event = serde_json::from_str(&line).expect("unknown kinds must stay readable");
        assert_eq!(event.event_type, "turn.compacted");
        assert_eq!(event.sequence, Some(7));

        // The payload survives intact, so a host can still inspect or forward
        // what it can't statically understand.
        let EventData::Custom {
            event_type,
            ref payload,
        } = event.data
        else {
            panic!("expected an unknown kind to degrade to Custom");
        };
        assert_eq!(event_type, "turn.compacted");
        assert_eq!(payload["removed"], 12);
        assert_eq!(payload["kind"], "turn_compacted");
    }

    #[test]
    fn known_event_kinds_still_deserialize_to_their_typed_variant() {
        let request = EventRequest::new(
            SessionId::new(),
            EventData::ToolCompleted {
                call_id: "call_0".into(),
                name: "read_file".into(),
                output: "hi".into(),
                is_error: false,
            },
        );
        let event = request.into_event(EventId::new(), Utc::now(), 3);
        let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(back, event);
        assert!(matches!(back.data, EventData::ToolCompleted { .. }));
    }

    #[test]
    fn custom_event_carries_its_own_type_and_is_durable() {
        let data = EventData::custom("budget.warning", serde_json::json!({"remaining": 10}));
        assert_eq!(data.event_type(), "budget.warning");
        assert!(!data.is_ephemeral());

        let event =
            EventRequest::new(SessionId::new(), data).into_event(EventId::new(), Utc::now(), 1);
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
        assert_eq!(back.event_type, "budget.warning");
    }
}
