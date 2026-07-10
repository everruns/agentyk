//! Smallest possible agentyk program: compose an agent by value, run a turn,
//! inspect the event log. Runs offline via the scripted sim driver:
//!
//! ```sh
//! cargo run --example hello
//! ```

use agentyk::{Agent, FnTool, ModelSpec, SimDriver, SimTurn, ToolOutput};
use serde_json::json;

#[tokio::main]
async fn main() -> agentyk::Result<()> {
    let agent = Agent::builder()
        .name("hello")
        .system_prompt("You are terse.")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("time", json!({})),
            SimTurn::text("It is exactly now."),
        ]))
        .tool(FnTool::new(
            "time",
            "Current time.",
            json!({"type": "object"}),
            |_| async { ToolOutput::text(chrono::Utc::now().to_rfc3339()) },
        ))
        .build()?;

    let mut session = agent.session();
    let turn = session.run("what time is it?").await?;

    println!("response: {}", turn.response);
    println!(
        "iterations: {}, tool calls: {}",
        turn.iterations, turn.tool_calls
    );
    println!("\nevent log:");
    for event in session.events().await? {
        println!("  {:>2}. {}", event.sequence, event.event_type);
    }
    Ok(())
}
