//! The transcript surface is just an `EventListener` — no core change, no
//! place in the turn loop.

use std::sync::Arc;

use agentyk::{Agent, FnTool, ModelSpec, Result, SimDriver, SimTurn, ToolOutput};
use agentyk_everruns::NarrationListener;
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
