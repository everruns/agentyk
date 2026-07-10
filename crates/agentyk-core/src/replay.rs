//! Replay — fold an event log back into message history.
//!
//! History is *derived*, never stored: any host that persists events can
//! reconstruct a session (including mid-turn, for durable execution) with
//! [`messages_from_events`]. This fold is the invariant the whole
//! persistence story rests on.

use crate::event::{Event, EventData};
use crate::message::Message;

/// The message a recorded event contributes to history, if any. Ephemeral
/// events (`output.message.delta`) never contribute — only the completed
/// message does, so replay is identical whether or not deltas were emitted.
pub fn message_from_event_data(data: &EventData) -> Option<Message> {
    match data {
        EventData::InputMessage { message } | EventData::OutputMessageCompleted { message } => {
            Some(message.clone())
        }
        EventData::ToolCompleted {
            call_id, output, ..
        } => Some(Message::tool_result(call_id.clone(), output.clone())),
        _ => None,
    }
}

/// Rebuild message history from a session's events, ordered by sequence.
/// Ephemeral events (`sequence: None`) sort first and contribute nothing —
/// in practice they are never persisted, so `events` here holds durable
/// events only.
pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|e| e.sequence);
    ordered
        .into_iter()
        .filter_map(|event| message_from_event_data(&event.data))
        .collect()
}
