//! Local file-backed event log: one JSON-serialized [`Event`] per line.
//!
//! This is a simple single-process store for examples, development, and local
//! resume. It flushes Rust's userspace buffer after each batch, but does not
//! provide cross-process locking, fsync durability, owner-only permissions,
//! corrupt-tail repair, or physical branch filtering. Production hosts should
//! implement [`agentyk_core::event_log::EventStore`] over their existing
//! durable session log.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use agentyk_core::error::{Error, Result};
use agentyk_core::event::{Event, EventRequest};
use agentyk_core::event_log::{EventLog, EventPage, EventRange, ExpectedVersion, SessionPoint};
use agentyk_core::id::{EventId, SessionId};
use async_trait::async_trait;
use chrono::Utc;

struct JsonlState {
    writer: BufWriter<std::fs::File>,
    sequences: HashMap<SessionId, u64>,
}

/// Multiple sessions may share one local-process file; sequences are tracked
/// per session. Reopening an existing file resumes sequence numbering from
/// its contents.
pub struct JsonlEventLog {
    path: PathBuf,
    state: Mutex<JsonlState>,
}

impl JsonlEventLog {
    /// Open or create a log at `path`, creating parent directories as
    /// needed. An existing file is read once so sequence numbering resumes
    /// where it left off; a corrupt line fails the open rather than silently
    /// restarting a session's numbering.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut sequences = HashMap::new();
        if path.exists() {
            let reader = BufReader::new(std::fs::File::open(&path)?);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let event: Event = serde_json::from_str(&line)
                    .map_err(|e| Error::EventLog(format!("corrupt log line: {e}")))?;
                // Every persisted line is durable (ephemeral events never
                // reach an EventLog), so `sequence` is always `Some` here.
                let sequence = event.sequence.ok_or_else(|| {
                    Error::EventLog(format!(
                        "log line for session {} has no sequence",
                        event.session_id
                    ))
                })?;
                let seq = sequences.entry(event.session_id).or_insert(0);
                *seq = (*seq).max(sequence);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            state: Mutex::new(JsonlState {
                writer: BufWriter::new(file),
                sequences,
            }),
        })
    }

    /// Where this log is written.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl EventLog for JsonlEventLog {
    async fn append_batch(
        &self,
        session_id: SessionId,
        expected: ExpectedVersion,
        requests: Vec<EventRequest>,
    ) -> Result<Vec<Event>> {
        let mut state = self.state.lock().expect("event log poisoned");
        let actual = state.sequences.get(&session_id).copied().unwrap_or(0);
        if let ExpectedVersion::Exact(expected) = expected
            && expected != actual
        {
            return Err(Error::EventConflict { expected, actual });
        }
        if requests
            .iter()
            .any(|request| request.session_id != session_id)
        {
            return Err(Error::EventLog(
                "event batch contains a different session".into(),
            ));
        }

        let now = Utc::now();
        let events: Vec<Event> = requests
            .into_iter()
            .enumerate()
            .map(|(index, request)| {
                request.into_event(EventId::new(), now, actual + index as u64 + 1)
            })
            .collect();
        let mut encoded = Vec::new();
        for event in &events {
            serde_json::to_writer(&mut encoded, event)?;
            encoded.push(b'\n');
        }
        state.writer.write_all(&encoded)?;
        state.writer.flush()?;
        if !events.is_empty() {
            state
                .sequences
                .insert(session_id, actual + events.len() as u64);
        }
        Ok(events)
    }

    async fn read_after(&self, session_id: SessionId, sequence: Option<u64>) -> Result<Vec<Event>> {
        // Hold the lock so appends can't interleave with the read.
        let _state = self.state.lock().expect("event log poisoned");
        let reader = BufReader::new(std::fs::File::open(&self.path)?);
        let after = sequence.unwrap_or(0);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)
                .map_err(|e| Error::EventLog(format!("corrupt log line: {e}")))?;
            if event.session_id == session_id && event.sequence.is_some_and(|value| value > after) {
                events.push(event);
            }
        }
        events.sort_by_key(|e| e.sequence);
        Ok(events)
    }

    async fn head(&self, session_id: SessionId) -> Result<SessionPoint> {
        let state = self.state.lock().expect("event log poisoned");
        Ok(SessionPoint::new(
            session_id,
            state.sequences.get(&session_id).copied().unwrap_or(0),
        ))
    }

    async fn read_page(&self, range: EventRange) -> Result<EventPage> {
        let state = self.state.lock().expect("event log poisoned");
        let head = SessionPoint::new(
            range.session_id,
            state.sequences.get(&range.session_id).copied().unwrap_or(0),
        );
        let reader = BufReader::new(std::fs::File::open(&self.path)?);
        let after = range.after.unwrap_or(0);
        let through = range.through.unwrap_or(u64::MAX);
        let mut events = Vec::new();
        let mut has_more = false;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)
                .map_err(|e| Error::EventLog(format!("corrupt log line: {e}")))?;
            if event.session_id != range.session_id
                || !event
                    .sequence
                    .is_some_and(|sequence| sequence > after && sequence <= through)
            {
                continue;
            }
            if events.len() == range.limit {
                has_more = true;
                break;
            }
            events.push(event);
        }
        let next = has_more
            .then(|| events.last().and_then(|event| event.sequence))
            .flatten()
            .map(|sequence| SessionPoint::new(range.session_id, sequence));
        Ok(EventPage { events, next, head })
    }
}
