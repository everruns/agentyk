//! The transcript: a fold of the agentyk event stream into renderable entries.
//!
//! The event log is the single source of truth, so the UI keeps no parallel
//! bookkeeping — it does not append the user's message on submit or guess when
//! a tool finished. [`Transcript::apply`] is the only way entries appear,
//! which makes the whole display testable without a terminal (see
//! `tests/transcript.rs`).

use agentyk::{Event, EventData, Message, Role};

/// What a gated or executed tool call is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// Started; either running or waiting on approval.
    Running,
    Ok,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// Assistant text. Grows in place as `output.message.delta` events arrive.
    Assistant(String),
    Tool {
        call_id: String,
        name: String,
        /// The arguments flattened to one line for display.
        summary: String,
        output: String,
        state: ToolState,
    },
    /// Out-of-band note: a cancelled turn, an error, a session banner.
    Notice(String),
}

#[derive(Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    /// Index of the assistant entry currently being streamed into, if any.
    streaming: Option<usize>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Notice(text.into()));
    }

    /// Fold one event into the transcript.
    pub fn apply(&mut self, event: &Event) {
        match &event.data {
            EventData::InputMessage { message } if message.role == Role::User => {
                self.entries.push(Entry::User(message.text()));
            }
            EventData::OutputMessageStarted { .. } => {
                // The assistant entry is created lazily on the first delta, so
                // a reason step that only calls tools adds no empty bubble.
                self.streaming = None;
            }
            EventData::OutputMessageDelta { accumulated, .. } => {
                self.stream(accumulated.clone());
            }
            EventData::OutputMessageCompleted { message, .. } => {
                self.finish_assistant(message);
            }
            EventData::ToolStarted { call } => {
                self.entries.push(Entry::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    summary: summarize_arguments(&call.arguments),
                    output: String::new(),
                    state: ToolState::Running,
                });
            }
            EventData::ToolCompleted {
                call_id,
                output,
                is_error,
                ..
            } => {
                if let Some(entry) = self.tool_mut(call_id) {
                    // A denial already set the state and the reason; the
                    // executor still records a `tool.completed` carrying the
                    // same text, so don't overwrite it.
                    if *entry.state != ToolState::Denied {
                        *entry.output = output.clone();
                        *entry.state = if *is_error {
                            ToolState::Failed
                        } else {
                            ToolState::Ok
                        };
                    }
                }
            }
            EventData::ToolDenied {
                call_id, reason, ..
            } => {
                if let Some(entry) = self.tool_mut(call_id) {
                    *entry.output = reason.clone();
                    *entry.state = ToolState::Denied;
                }
            }
            EventData::TurnFailed { error } => self.notice(format!("turn failed: {error}")),
            EventData::TurnCancelled => self.notice("interrupted"),
            EventData::TurnSealed { reason } => self.notice(format!("turn sealed: {reason:?}")),
            EventData::TurnStarted { .. } | EventData::TurnCompleted { .. } => {}
            EventData::InputMessage { .. } => {}
            EventData::Custom { event_type, .. } => self.notice(format!("event: {event_type}")),
            // `EventData` is non-exhaustive, and for a transcript that is the
            // right default: an event type this build predates should leave
            // the display alone rather than break it.
            _ => {}
        }
    }

    /// Upsert the streaming assistant entry with the accumulated text.
    fn stream(&mut self, accumulated: String) {
        match self.streaming {
            Some(index) => {
                if let Some(Entry::Assistant(text)) = self.entries.get_mut(index) {
                    *text = accumulated;
                    return;
                }
                // The entry moved or was replaced; start a fresh one.
                self.streaming = None;
                self.stream(accumulated);
            }
            None => {
                self.entries.push(Entry::Assistant(accumulated));
                self.streaming = Some(self.entries.len() - 1);
            }
        }
    }

    /// Reconcile the streamed text with the materialized message. Drivers
    /// without incremental streaming produce no deltas at all, so this is
    /// also where the assistant text first appears for them.
    fn finish_assistant(&mut self, message: &Message) {
        let text = message.text();
        match self.streaming.take() {
            Some(index) => match text.trim().is_empty() {
                // Tool-call-only step: drop the placeholder we opened.
                true => {
                    self.entries.remove(index);
                }
                false => {
                    if let Some(Entry::Assistant(existing)) = self.entries.get_mut(index) {
                        *existing = text;
                    }
                }
            },
            None if !text.trim().is_empty() => self.entries.push(Entry::Assistant(text)),
            None => {}
        }
    }

    /// The most recent tool entry for `call_id`. Reverse scan because the
    /// answer is almost always the entry just pushed.
    fn tool_mut(&mut self, call_id: &str) -> Option<ToolEntry<'_>> {
        self.entries.iter_mut().rev().find_map(|entry| match entry {
            Entry::Tool {
                call_id: id,
                output,
                state,
                ..
            } if id.as_str() == call_id => Some(ToolEntry { output, state }),
            _ => None,
        })
    }
}

/// A borrowed view of the mutable `Entry::Tool` fields, so callers update
/// named fields instead of re-matching the variant at every call site.
struct ToolEntry<'a> {
    output: &'a mut String,
    state: &'a mut ToolState,
}

/// Flatten tool arguments to a single display line: the bare value when
/// there's one obvious argument, `key=value` pairs otherwise.
pub fn summarize_arguments(arguments: &serde_json::Value) -> String {
    let Some(object) = arguments.as_object() else {
        return compact(&arguments.to_string());
    };
    // A one-argument call reads better bare: `ls -1` beats `command=ls -1`.
    if object.len() == 1
        && let Some(value) = object.values().next()
    {
        return compact(&scalar(value));
    }
    // Clip each value rather than the joined line: a `write_file` whose
    // `content` runs for pages would otherwise push `path` off the row, and the
    // path is the part worth reading.
    let pairs: Vec<String> = object
        .iter()
        .map(|(key, value)| format!("{key}={}", clip(&compact(&scalar(value)), VALUE_LIMIT)))
        .collect();
    compact(&pairs.join(" "))
}

/// Per-value ceiling in a multi-argument summary.
const VALUE_LIMIT: usize = 48;
/// Ceiling for the whole summary row.
const LINE_LIMIT: usize = 120;

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Collapse all whitespace to single spaces and bound the result.
fn compact(text: &str) -> String {
    clip(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        LINE_LIMIT,
    )
}

fn clip(text: &str, limit: usize) -> String {
    match text.chars().count() > limit {
        true => text.chars().take(limit - 1).chain(['…']).collect(),
        false => text.to_string(),
    }
}
