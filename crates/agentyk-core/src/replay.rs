//! Replay — fold an event log back into message history.
//!
//! History is *derived*, never stored: any host that persists events can
//! reconstruct a session (including mid-turn, for durable execution) with
//! [`History::from_events`]. This fold is the invariant the whole persistence
//! story rests on.
//!
//! A running turn needs the history *now*, not a log read per reason step, so
//! [`History`] also carries the live projection an executor advances as it
//! records. That is the only reason a second copy exists — and it is a
//! [`History`], not a `Vec<Message>`, precisely so it cannot become a second
//! *source*: the only way to grow one is [`History::apply`], which takes a
//! recorded [`EventData`]. A message that never became an event cannot enter
//! history, so the projection and a fresh replay of the log agree by
//! construction.

use crate::event::{Event, EventData};
use crate::message::Message;

/// The message a recorded event contributes to history, if any. Ephemeral
/// events (`output.message.delta`) never contribute — only the completed
/// message does, so replay is identical whether or not deltas were emitted.
pub fn message_from_event_data(data: &EventData) -> Option<Message> {
    match data {
        EventData::InputMessage { message } | EventData::OutputMessageCompleted { message, .. } => {
            Some(message.clone())
        }
        EventData::ToolCompleted {
            call_id,
            output,
            parts,
            ..
        } => Some(Message::tool_result_with_parts(
            call_id.clone(),
            output.clone(),
            parts.clone(),
        )),
        _ => None,
    }
}

/// A session's message history: the fold of its event log, and nothing else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    messages: Vec<Message>,
}

impl History {
    /// An empty history, for a session that has recorded nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild history from a session's events, ordered by sequence.
    /// Ephemeral events (`sequence: None`) sort first and contribute nothing
    /// — in practice they are never persisted, so `events` here holds durable
    /// events only.
    pub fn from_events(events: &[Event]) -> Self {
        let mut ordered: Vec<&Event> = events.iter().collect();
        ordered.sort_by_key(|e| e.sequence);
        Self {
            messages: ordered
                .into_iter()
                .filter_map(|event| message_from_event_data(&event.data))
                .collect(),
        }
    }

    /// Fold one recorded event into the live projection. The only way to grow
    /// a history — which is what keeps it equal to a replay of the log.
    /// Events that contribute no message (tool starts, turn lifecycle,
    /// streaming deltas) are no-ops.
    pub fn apply(&mut self, data: &EventData) {
        if let Some(message) = message_from_event_data(data) {
            self.messages.push(message);
        }
    }

    /// The conversation so far, oldest first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether no event has contributed a message yet.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// How many messages history holds.
    pub fn len(&self) -> usize {
        self.messages.len()
    }
}

/// Rebuild message history from a session's events — shorthand for
/// [`History::from_events`] when you only want the messages.
pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    History::from_events(events).messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EventId, MessageId, SessionId};
    use chrono::Utc;

    fn durable(session_id: SessionId, sequence: u64, data: EventData) -> Event {
        crate::event::EventRequest::new(session_id, data).into_event(
            EventId::new(),
            Utc::now(),
            sequence,
        )
    }

    /// The property the persistence story rests on: advancing the live
    /// projection event by event lands in the same place as replaying the log
    /// from scratch. If these could disagree, "replay is sufficient to resume"
    /// would be false.
    #[test]
    fn applying_events_one_by_one_matches_a_replay_of_the_same_log() {
        let session_id = SessionId::new();
        let events = vec![
            durable(
                session_id,
                1,
                EventData::InputMessage {
                    message: Message::user("hi"),
                },
            ),
            durable(session_id, 2, EventData::TurnStarted { max_iterations: 16 }),
            durable(
                session_id,
                3,
                EventData::OutputMessageCompleted {
                    message_id: MessageId::new(),
                    message: Message::assistant("thinking"),
                    usage: crate::driver::Usage::default(),
                },
            ),
            durable(
                session_id,
                4,
                EventData::ToolCompleted {
                    call_id: "call_0".into(),
                    name: "echo".into(),
                    output: "out".into(),
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    parts: Vec::new(),
                },
            ),
        ];

        let mut live = History::new();
        for event in &events {
            live.apply(&event.data);
        }

        assert_eq!(live, History::from_events(&events));
        // Only the three message-bearing events contribute.
        assert_eq!(live.len(), 3);
    }

    #[test]
    fn a_streaming_delta_contributes_nothing() {
        let mut history = History::new();
        history.apply(&EventData::OutputMessageDelta {
            message_id: MessageId::new(),
            delta: "hi".into(),
            accumulated: "hi".into(),
        });
        assert!(history.is_empty());
    }
}
