//! File-backed event log: one JSON-serialized [`Event`] per line — the
//! batteries-included durability option (the yolop pattern).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use agentyk_core::error::{Error, Result};
use agentyk_core::event::{Event, EventRequest};
use agentyk_core::event_log::EventLog;
use agentyk_core::id::{EventId, SessionId};
use async_trait::async_trait;
use chrono::Utc;

struct JsonlState {
    writer: BufWriter<std::fs::File>,
    sequences: HashMap<SessionId, u64>,
}

/// Multiple sessions may share one file; sequences are tracked per session.
/// Reopening an existing file resumes sequence numbering from its contents.
pub struct JsonlEventLog {
    path: PathBuf,
    state: Mutex<JsonlState>,
}

impl JsonlEventLog {
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
                let seq = sequences.entry(event.session_id).or_insert(0);
                *seq = (*seq).max(event.sequence);
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

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl EventLog for JsonlEventLog {
    async fn append(&self, request: EventRequest) -> Result<Event> {
        let mut state = self.state.lock().expect("event log poisoned");
        let seq = state.sequences.entry(request.session_id).or_insert(0);
        *seq += 1;
        let sequence = *seq;
        let event = request.into_event(EventId::new(), Utc::now(), sequence);
        let line = serde_json::to_string(&event)?;
        state.writer.write_all(line.as_bytes())?;
        state.writer.write_all(b"\n")?;
        state.writer.flush()?;
        Ok(event)
    }

    async fn read(&self, session_id: SessionId) -> Result<Vec<Event>> {
        // Hold the lock so appends can't interleave with the read.
        let _state = self.state.lock().expect("event log poisoned");
        let reader = BufReader::new(std::fs::File::open(&self.path)?);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)
                .map_err(|e| Error::EventLog(format!("corrupt log line: {e}")))?;
            if event.session_id == session_id {
                events.push(event);
            }
        }
        events.sort_by_key(|e| e.sequence);
        Ok(events)
    }
}
