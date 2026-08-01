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
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::event::{Event, EventData, EventRequest};
use crate::id::{EventId, SessionId};

/// Immutable address of a durable point in a session timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionPoint {
    /// Session whose timeline is addressed.
    pub session_id: SessionId,
    /// Inclusive durable sequence, with zero denoting an empty session.
    pub sequence: u64,
}

impl SessionPoint {
    /// Address `sequence` in `session_id`.
    pub fn new(session_id: SessionId, sequence: u64) -> Self {
        Self {
            session_id,
            sequence,
        }
    }
}

/// Bounded forward read over one session timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRange {
    /// Session to read.
    pub session_id: SessionId,
    /// Exclude events at or before this sequence.
    pub after: Option<u64>,
    /// Exclude events after this sequence.
    pub through: Option<u64>,
    /// Maximum events returned in one page.
    pub limit: usize,
}

impl EventRange {
    /// Read the first page of `session_id` with a conservative default size.
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            after: None,
            through: None,
            limit: 256,
        }
    }

    /// Start strictly after `sequence`.
    pub fn after(mut self, sequence: u64) -> Self {
        self.after = Some(sequence);
        self
    }

    /// Stop at `sequence`, inclusively.
    pub fn through(mut self, sequence: u64) -> Self {
        self.through = Some(sequence);
        self
    }

    /// Return at most `limit` events. Zero returns an empty page.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// One bounded event read plus cursors needed to continue consistently.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    /// Events in ascending durable sequence order.
    pub events: Vec<Event>,
    /// Cursor for the next page when more events remain in the requested range.
    pub next: Option<SessionPoint>,
    /// Stream head observed while serving this page.
    pub head: SessionPoint,
}

/// Disposable projection anchored to an immutable session point.
///
/// Events remain authoritative. A snapshot only accelerates replay and may be
/// dropped whenever its projection name or schema is no longer understood.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionSnapshot {
    /// Last event incorporated into the projection.
    pub point: SessionPoint,
    /// Stable projection family, such as `history` or `turn_state`.
    pub projection: String,
    /// Schema understood by the projection reducer.
    pub schema_version: u32,
    /// Projection-owned serialized state.
    pub payload: serde_json::Value,
}

/// Optional storage for disposable replay accelerators.
#[async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Most recent compatible snapshot at or before `through`.
    async fn load_latest(
        &self,
        session_id: SessionId,
        projection: &str,
        schema_version: u32,
        through: Option<u64>,
    ) -> Result<Option<ProjectionSnapshot>>;

    /// Save or replace a snapshot at its immutable session point.
    async fn save(&self, snapshot: ProjectionSnapshot) -> Result<()>;
}

/// In-memory snapshot storage for tests and short-lived applications.
#[derive(Default)]
pub struct InMemorySnapshotStore {
    snapshots: Mutex<HashMap<(SessionId, String), Vec<ProjectionSnapshot>>>,
}

impl InMemorySnapshotStore {
    /// Empty snapshot storage.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SnapshotStore for InMemorySnapshotStore {
    async fn load_latest(
        &self,
        session_id: SessionId,
        projection: &str,
        schema_version: u32,
        through: Option<u64>,
    ) -> Result<Option<ProjectionSnapshot>> {
        let through = through.unwrap_or(u64::MAX);
        let snapshots = self.snapshots.lock().expect("snapshot store poisoned");
        Ok(snapshots
            .get(&(session_id, projection.to_owned()))
            .into_iter()
            .flatten()
            .filter(|snapshot| {
                snapshot.schema_version == schema_version && snapshot.point.sequence <= through
            })
            .max_by_key(|snapshot| snapshot.point.sequence)
            .cloned())
    }

    async fn save(&self, snapshot: ProjectionSnapshot) -> Result<()> {
        let key = (snapshot.point.session_id, snapshot.projection.clone());
        let mut snapshots = self.snapshots.lock().expect("snapshot store poisoned");
        let entries = snapshots.entry(key).or_default();
        if let Some(existing) = entries.iter_mut().find(|existing| {
            existing.point.sequence == snapshot.point.sequence
                && existing.schema_version == snapshot.schema_version
        }) {
            *existing = snapshot;
        } else {
            entries.push(snapshot);
        }
        Ok(())
    }
}

/// Required stream version when appending a durable event batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedVersion {
    /// Append regardless of the current stream version.
    Any,
    /// Append only when the stream currently ends at this sequence.
    Exact(u64),
}

/// Append-only, per-session-sequenced event storage.
///
/// This is the production persistence seam, not a prescription for a file or
/// database layout. A Yolop-class implementation can place logical branches
/// over an existing session log, use database transactions or cross-process
/// locks for [`ExpectedVersion`], fsync before acknowledging an append,
/// enforce owner-only access, and repair a truncated physical tail. The
/// contract visible to the engine stays the same: atomic batches, contiguous
/// effective sequences, ordered branch reads, and immutable fork points.
///
/// Physical recovery and access control remain store responsibilities because
/// the correct mechanism depends on the backing system. Returning `Ok` from
/// [`EventStore::append_batch`] means the implementation has reached whatever
/// durability level it documents for its host.
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

    /// Current durable head without loading the stream body.
    ///
    /// Implementations should override this when they can answer from an
    /// index. The default preserves compatibility for simple stores.
    async fn head(&self, session_id: SessionId) -> Result<SessionPoint> {
        let sequence = self
            .read_after(session_id, None)
            .await?
            .last()
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        Ok(SessionPoint::new(session_id, sequence))
    }

    /// Read one bounded page. Implementations should push bounds into their
    /// storage query; the default is correct for small custom stores.
    async fn read_page(&self, range: EventRange) -> Result<EventPage> {
        let head = self.head(range.session_id).await?;
        let mut events = self.read_after(range.session_id, range.after).await?;
        if let Some(through) = range.through {
            events.retain(|event| event.sequence.is_some_and(|sequence| sequence <= through));
        }
        let has_more = events.len() > range.limit;
        events.truncate(range.limit);
        let next = has_more
            .then(|| events.last().and_then(|event| event.sequence))
            .flatten()
            .map(|sequence| SessionPoint::new(range.session_id, sequence));
        Ok(EventPage { events, next, head })
    }

    /// Read an immutable prefix through `point`, using bounded pages.
    async fn read_through(&self, point: SessionPoint) -> Result<Vec<Event>> {
        let head = self.head(point.session_id).await?;
        if point.sequence > head.sequence {
            return Err(crate::error::Error::EventLog(format!(
                "point {} is past session head {}",
                point.sequence, head.sequence
            )));
        }
        let mut events = Vec::new();
        let mut after = None;
        loop {
            let mut range = EventRange::new(point.session_id).through(point.sequence);
            if let Some(sequence) = after {
                range = range.after(sequence);
            }
            let page = self.read_page(range).await?;
            events.extend(page.events);
            match page.next {
                Some(next) => after = Some(next.sequence),
                None => return Ok(events),
            }
        }
    }

    /// Create a new session whose initial history is the prefix ending at
    /// `origin`. The returned id is generated by the store.
    ///
    /// This default materializes the prefix. Database implementations may use
    /// copy-on-write lineage as long as reads expose the same effective stream.
    async fn fork(&self, origin: SessionPoint) -> Result<SessionId> {
        let parent_head = self.head(origin.session_id).await?;
        if origin.sequence > parent_head.sequence {
            return Err(crate::error::Error::EventLog(format!(
                "fork point {} is past session head {}",
                origin.sequence, parent_head.sequence
            )));
        }
        let child = SessionId::new();
        let inherited = self.read_through(origin).await?.into_iter();
        let mut requests: Vec<EventRequest> = inherited
            .map(|event| EventRequest {
                session_id: child,
                turn_id: event.turn_id,
                metadata: event.metadata,
                data: event.data,
            })
            .collect();
        requests.push(EventRequest::new(
            child,
            EventData::SessionForked {
                parent_session_id: origin.session_id,
                parent_sequence: origin.sequence,
            },
        ));
        self.append_batch(child, ExpectedVersion::Exact(0), requests)
            .await?;
        Ok(child)
    }

    /// Append one event with optimistic concurrency against the current
    /// stream version.
    async fn append(&self, request: EventRequest) -> Result<Event> {
        let session_id = request.session_id;
        let version = self.head(session_id).await?.sequence;
        self.append_batch(session_id, ExpectedVersion::Exact(version), vec![request])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::Error::EventLog("append returned no event".into()))
    }

    /// All events for a session, ordered by sequence.
    async fn read(&self, session_id: SessionId) -> Result<Vec<Event>> {
        let head = self.head(session_id).await?;
        self.read_through(head).await
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

    async fn head(&self, session_id: SessionId) -> Result<SessionPoint> {
        let sessions = self.sessions.lock().expect("event log poisoned");
        let sequence = sessions
            .get(&session_id)
            .and_then(|events| events.last())
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        Ok(SessionPoint::new(session_id, sequence))
    }

    async fn read_page(&self, range: EventRange) -> Result<EventPage> {
        let sessions = self.sessions.lock().expect("event log poisoned");
        let stream = sessions.get(&range.session_id);
        let head_sequence = stream
            .and_then(|events| events.last())
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        let after = range.after.unwrap_or(0);
        let through = range.through.unwrap_or(u64::MAX);
        let mut matching = stream.into_iter().flatten().filter(|event| {
            event
                .sequence
                .is_some_and(|sequence| sequence > after && sequence <= through)
        });
        let events: Vec<Event> = matching.by_ref().take(range.limit).cloned().collect();
        let next = matching
            .next()
            .and_then(|_| events.last())
            .and_then(|event| event.sequence)
            .map(|sequence| SessionPoint::new(range.session_id, sequence));
        Ok(EventPage {
            events,
            next,
            head: SessionPoint::new(range.session_id, head_sequence),
        })
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
