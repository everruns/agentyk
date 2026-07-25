//! Narration — human-readable transcript lines from the event stream.
//!
//! everruns' largest UI surface (the TUI transcript, `tool_narration`) is, in
//! agentyk terms, just an [`EventListener`]: it observes the same events the
//! log records and renders them. This shows that surface needs no core change
//! and no place in the turn loop — it's a pure observer.
//!
//! It also renders the everruns-flavored richness the satellite adds — tool
//! **risk hints** and pre-run **redaction** (both carried as `EventData::Custom`
//! events the [`crate::EverrunsExecutor`] emits) and provider **extended
//! thinking** (the typed [`Message::thinking`](agentyk_core::message::Message)
//! field) — still without observing anything but events.

use std::sync::Mutex;

use agentyk_core::event::{Event, EventData, EventListener};
use async_trait::async_trait;

/// Collects readable narration lines from the event stream. A real host would
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

    /// Render the lines a single event contributes (possibly none, or — for a
    /// reasoning step with thinking — more than one).
    fn narrate(data: &EventData) -> Vec<String> {
        match data {
            EventData::TurnStarted => vec!["• turn started".to_string()],
            EventData::InputMessage { message } => vec![format!("› {}", message.text())],
            EventData::OutputMessageCompleted { message, .. } => {
                let mut lines = Vec::new();
                if let Some(thinking) = &message.thinking {
                    lines.push(format!("💭 {}", summarize(thinking)));
                }
                let text = message.text();
                if !text.is_empty() {
                    lines.push(format!("‹ {text}"));
                }
                lines // empty vec = a tool-call-only step; the tool.* lines cover it
            }
            EventData::ToolStarted { call } => vec![format!("⚙ {}(…)", call.name)],
            EventData::ToolCompleted { name, is_error, .. } => {
                vec![format!("{} {name}", if *is_error { "✗" } else { "✓" })]
            }
            EventData::ToolDenied { name, reason, .. } => {
                vec![format!("⛔ {name} denied — {reason}")]
            }
            // Rewriting became a first-class core event once middleware could
            // express it, so this no longer reads a custom event.
            EventData::ToolRewritten { name, .. } => {
                vec![format!("✎ {name} — redacted before it ran")]
            }
            // everruns-flavored richness the satellite emits as custom events.
            EventData::Custom {
                event_type,
                payload,
            } => match event_type.as_str() {
                "tool.hint" => {
                    let name = payload["name"].as_str().unwrap_or("?");
                    let risk = payload["risk"].as_str().unwrap_or("");
                    vec![format!("{} {name} — {risk}", risk_icon(risk))]
                }
                _ => vec![],
            },
            EventData::TurnCompleted { .. } => vec!["• turn completed".to_string()],
            _ => vec![],
        }
    }
}

/// A risk-specific glyph for a tool hint line.
fn risk_icon(risk: &str) -> &'static str {
    match risk {
        "destructive" => "⚠",
        "open-world" => "🌐",
        _ => "🔎",
    }
}

/// Collapse thinking to a single short line for the transcript.
fn summarize(thinking: &str) -> String {
    let one_line = thinking.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 60 {
        format!("{}…", one_line.chars().take(60).collect::<String>())
    } else {
        one_line
    }
}

#[async_trait]
impl EventListener for NarrationListener {
    async fn on_event(&self, event: &Event) {
        let rendered = Self::narrate(&event.data);
        if !rendered.is_empty() {
            self.lines.lock().unwrap().extend(rendered);
        }
    }

    fn name(&self) -> &'static str {
        "narration"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk_core::message::Message;

    #[test]
    fn renders_thinking_before_the_answer() {
        let message = Message::assistant("here is the answer")
            .with_thinking("let me work through this step by step", None);
        let lines = NarrationListener::narrate(&EventData::OutputMessageCompleted {
            message,
            message_id: Default::default(),
        });
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("💭 "));
        assert_eq!(lines[1], "‹ here is the answer");
    }

    #[test]
    fn renders_the_hint_custom_event() {
        let hint = NarrationListener::narrate(&EventData::custom(
            "tool.hint",
            serde_json::json!({ "name": "delete_all", "risk": "destructive" }),
        ));
        assert_eq!(hint, vec!["⚠ delete_all — destructive".to_string()]);
    }

    #[test]
    fn renders_the_first_class_rewrite_event() {
        let rewrite = NarrationListener::narrate(&EventData::ToolRewritten {
            call_id: "call_0".into(),
            name: "save_note".into(),
            by: Some("everruns-guard".into()),
        });
        assert_eq!(
            rewrite,
            vec!["✎ save_note — redacted before it ran".to_string()]
        );
    }

    #[test]
    fn ignores_unknown_custom_events() {
        let lines = NarrationListener::narrate(&EventData::custom(
            "budget.warning",
            serde_json::Value::Null,
        ));
        assert!(lines.is_empty());
    }
}
