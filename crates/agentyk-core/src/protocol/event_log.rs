//! Event log — the append-only record of everything a session did, and the
//! persistence seam of the library.
//!
//! Sessions write through [`EventLog`] and can be resumed by replaying it
//! (see [`crate::replay`]). Hosts choose durability by choosing an
//! implementation: [`InMemoryEventLog`] ships here; `agentyk` adds a JSONL
//! file log; a server host can implement this trait over a database.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;

use crate::error::Result;
use crate::event::{Event, EventRequest};
use crate::id::{EventId, SessionId};

/// Append-only, per-session-sequenced event storage.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Append an event; the log assigns `id`, `ts`, and the per-session
    /// `sequence`, and returns the finalized event.
    async fn append(&self, request: EventRequest) -> Result<Event>;

    /// All events for a session, ordered by sequence.
    async fn read(&self, session_id: SessionId) -> Result<Vec<Event>>;
}

/// In-memory event log. The default for `Agent::session()` and the natural
/// choice for tests.
#[derive(Default)]
pub struct InMemoryEventLog {
    sessions: Mutex<HashMap<SessionId, Vec<Event>>>,
}

impl InMemoryEventLog {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventLog for InMemoryEventLog {
    async fn append(&self, request: EventRequest) -> Result<Event> {
        let mut sessions = self.sessions.lock().expect("event log poisoned");
        let events = sessions.entry(request.session_id).or_default();
        let sequence = events.len() as u64 + 1;
        let event = request.into_event(EventId::new(), Utc::now(), sequence);
        events.push(event.clone());
        Ok(event)
    }

    async fn read(&self, session_id: SessionId) -> Result<Vec<Event>> {
        let sessions = self.sessions.lock().expect("event log poisoned");
        Ok(sessions.get(&session_id).cloned().unwrap_or_default())
    }
}
