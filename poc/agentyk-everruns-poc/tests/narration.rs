//! The transcript surface is just an `EventListener` — no core change, no
//! place in the turn loop.

use std::sync::Arc;

use agentyk::{Agent, FnTool, ModelSpec, Result, SimDriver, SimToolCall, SimTurn, ToolOutput};
use agentyk_core::middleware::{ToolCallDecision, ToolInvocation, TurnMiddleware};
use agentyk_everruns_poc::{EverrunsExecutor, HintedTool, NarrationListener, ToolHints};
use async_trait::async_trait;
use serde_json::json;

#[tokio::test]
async fn narration_renders_the_event_stream() -> Result<()> {
    let narration: Arc<NarrationListener> = Arc::default();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("look", json!({})),
            SimTurn::text("all set"),
        ]))
        .listener_arc(narration.clone())
        .tool(FnTool::new(
            "look",
            "Look around.",
            json!({"type": "object"}),
            |_a| async move { ToolOutput::text("ok") },
        ))
        .build()?;

    agent.run("go").await?;

    let lines = narration.lines();
    // A readable transcript, entirely from observing events.
    assert!(lines.iter().any(|l| l.contains("turn started")));
    assert!(lines.iter().any(|l| l.contains("› go")));
    assert!(lines.iter().any(|l| l.contains("⚙ look")));
    assert!(lines.iter().any(|l| l.starts_with("✓ look")));
    assert!(lines.iter().any(|l| l.contains("‹ all set")));
    assert!(lines.iter().any(|l| l.contains("turn completed")));
    Ok(())
}

/// Redacts a `secret` argument before any tool runs — makes the executor emit
/// a `tool.rewritten` event.
struct RedactSecret;

#[async_trait]
impl TurnMiddleware for RedactSecret {
    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if invocation.call.arguments.get("secret").is_some() {
            let mut call = invocation.call.clone();
            call.arguments["secret"] = json!("***");
            return ToolCallDecision::Rewrite(call);
        }
        ToolCallDecision::Proceed
    }
}

fn echo(name: &'static str) -> FnTool {
    FnTool::new(name, name, json!({"type": "object"}), move |a| async move {
        ToolOutput::text(format!("{name} {a}"))
    })
}

#[tokio::test]
async fn narration_surfaces_hints_and_redaction_from_custom_events() -> Result<()> {
    let narration: Arc<NarrationListener> = Arc::default();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_calls([
                SimToolCall::new("search", json!({"q": "x"})),
                SimToolCall::new("save", json!({"secret": "p@ss"})),
            ]),
            SimTurn::text("done"),
        ]))
        // Only a redactor in the chain — no approval guard, so the destructive
        // tool still runs; the readonly/destructive hints and the redaction
        // ride the event stream as custom events regardless.
        .executor(EverrunsExecutor)
        .middleware(RedactSecret)
        .listener_arc(narration.clone())
        .tool(HintedTool::new(echo("search"), ToolHints::readonly()))
        .tool(HintedTool::new(echo("save"), ToolHints::destructive()))
        .build()?;

    agent.run("search then save a secret").await?;

    let lines = narration.lines();
    // Risk hints (from tool.hint custom events)...
    assert!(
        lines.iter().any(|l| l == "🔎 search — readonly"),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "⚠ save — destructive"),
        "{lines:?}"
    );
    // ...and the redaction (from a tool.rewritten custom event).
    assert!(
        lines.iter().any(|l| l == "✎ save — redacted before it ran"),
        "{lines:?}"
    );
    Ok(())
}
