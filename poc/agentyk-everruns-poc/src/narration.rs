//! Narration — human-readable transcript lines from the event stream.
//!
//! everruns' largest UI surface (the TUI transcript, `tool_narration`) is, in
//! agentyk terms, just an [`EventListener`]: it observes the same events the
//! log records and renders them. This shows that surface needs no core change
//! and no place in the turn loop — it's a pure observer.

use std::sync::Mutex;

use agentyk_core::event::{Event, EventData, EventListener};
use async_trait::async_trait;

/// Collects a readable narration line per meaningful event. A real host would
/// render these to a terminal; here they're captured so the behavior is
/// testable. Streaming deltas are ignored (the completed message is narrated
/// instead), matching how a transcript coalesces a stream into one line.
#[derive(Default)]
pub struct NarrationListener {
    lines: Mutex<Vec<String>>,
}

impl NarrationListener {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the narration so far.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn narrate(data: &EventData) -> Option<String> {
        match data {
            EventData::TurnStarted => Some("• turn started".to_string()),
            EventData::InputMessage { message } => Some(format!("› {}", message.text())),
            EventData::OutputMessageCompleted { message, .. } => {
                let text = message.text();
                if text.is_empty() {
                    None // a tool-call-only step; the tool.* lines cover it
                } else {
                    Some(format!("‹ {text}"))
                }
            }
            EventData::ToolStarted { call } => Some(format!("⚙ {}(…)", call.name)),
            EventData::ToolCompleted { name, is_error, .. } => {
                Some(format!("{} {name}", if *is_error { "✗" } else { "✓" }))
            }
            EventData::ToolDenied { name, reason, .. } => {
                Some(format!("⛔ {name} denied — {reason}"))
            }
            EventData::TurnCompleted { .. } => Some("• turn completed".to_string()),
            _ => None,
        }
    }
}

#[async_trait]
impl EventListener for NarrationListener {
    async fn on_event(&self, event: &Event) {
        if let Some(line) = Self::narrate(&event.data) {
            self.lines.lock().unwrap().push(line);
        }
    }

    fn name(&self) -> &'static str {
        "narration"
    }
}
