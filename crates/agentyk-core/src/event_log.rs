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

/// Required stream version when appending a durable event batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedVersion {
    /// Append regardless of the current stream version.
    Any,
    /// Append only when the stream currently ends at this sequence.
    Exact(u64),
}

/// Append-only, per-session-sequenced event storage.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Atomically append a batch and assign ids, timestamps, and contiguous
    /// per-session sequences.
    ///
    /// Every request must belong to `session_id`. An exact version prevents
    /// two hosts from advancing the same session concurrently.
    async fn append_batch(
        &self,
        session_id: SessionId,
        expected: ExpectedVersion,
        requests: Vec<EventRequest>,
    ) -> Result<Vec<Event>>;

    /// Read events after `sequence`, ordered by sequence.
    ///
    /// `None` reads the stream from its beginning.
    async fn read_after(&self, session_id: SessionId, sequence: Option<u64>) -> Result<Vec<Event>>;

    /// Append one event with optimistic concurrency against the current
    /// stream version.
    async fn append(&self, request: EventRequest) -> Result<Event> {
        let session_id = request.session_id;
        let version = self
            .read_after(session_id, None)
            .await?
            .last()
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        self.append_batch(session_id, ExpectedVersion::Exact(version), vec![request])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::Error::EventLog("append returned no event".into()))
    }

    /// All events for a session, ordered by sequence.
    async fn read(&self, session_id: SessionId) -> Result<Vec<Event>> {
        self.read_after(session_id, None).await
    }
}

/// In-memory event log. The default for `Agent::session()` and the natural
/// choice for tests.
#[derive(Default)]
pub struct InMemoryEventLog {
    sessions: Mutex<HashMap<SessionId, Vec<Event>>>,
}

impl InMemoryEventLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Compatibility name for [`EventStore`].
pub use EventStore as EventLog;

#[async_trait]
impl EventStore for InMemoryEventLog {
    async fn append_batch(
        &self,
        session_id: SessionId,
        expected: ExpectedVersion,
        requests: Vec<EventRequest>,
    ) -> Result<Vec<Event>> {
        let mut sessions = self.sessions.lock().expect("event log poisoned");
        let stream = sessions.entry(session_id).or_default();
        let actual = stream.len() as u64;
        if let ExpectedVersion::Exact(expected) = expected
            && expected != actual
        {
            return Err(crate::error::Error::EventConflict { expected, actual });
        }
        if requests
            .iter()
            .any(|request| request.session_id != session_id)
        {
            return Err(crate::error::Error::EventLog(
                "event batch contains a different session".into(),
            ));
        }
        let now = Utc::now();
        let appended: Vec<Event> = requests
            .into_iter()
            .enumerate()
            .map(|(index, request)| {
                request.into_event(EventId::new(), now, actual + index as u64 + 1)
            })
            .collect();
        stream.extend(appended.iter().cloned());
        Ok(appended)
    }

    async fn read_after(&self, session_id: SessionId, sequence: Option<u64>) -> Result<Vec<Event>> {
        let sessions = self.sessions.lock().expect("event log poisoned");
        let after = sequence.unwrap_or(0);
        Ok(sessions
            .get(&session_id)
            .into_iter()
            .flatten()
            .filter(|event| event.sequence.is_some_and(|value| value > after))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventData;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    #[test]
    fn batch_append_is_atomic_and_version_checked() {
        block_on(async {
            let log = InMemoryEventLog::new();
            let session_id = SessionId::new();
            let requests = vec![
                EventRequest::new(session_id, EventData::TurnStarted { max_iterations: 4 }),
                EventRequest::new(session_id, EventData::TurnCancelled),
            ];
            let appended = log
                .append_batch(session_id, ExpectedVersion::Exact(0), requests)
                .await
                .unwrap();
            assert_eq!(appended.len(), 2);
            assert_eq!(appended[0].sequence, Some(1));
            assert_eq!(appended[1].sequence, Some(2));

            let error = log
                .append_batch(
                    session_id,
                    ExpectedVersion::Exact(0),
                    vec![EventRequest::new(session_id, EventData::TurnCancelled)],
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                crate::error::Error::EventConflict {
                    expected: 0,
                    actual: 2
                }
            ));
            assert_eq!(log.read_after(session_id, Some(1)).await.unwrap().len(), 1);
        });
    }
}
