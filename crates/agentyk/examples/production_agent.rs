//! A production-shaped agent assembled from Agentyk's core building blocks.
//!
//! This example stays deterministic and offline with [`SimDriver`], while
//! showing the same composition used with a network driver: a typed tool, a
//! safety hook, a durable event log, multiple turns, and replay-based resume.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p agentyk --example production_agent
//! ```

use std::sync::Arc;

use agentyk::{
    Agent, Hook, HookEvent, HookMatcher, HookOutcome, HookPayload, JsonlEventLog, ModelSpec,
    Result, SimDriver, SimTurn, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;

/// Allows the read-only lookup tool while demonstrating where a host can
/// require approval or reject mutating tools.
struct ReadOnlyGate {
    matcher: HookMatcher,
}

#[async_trait]
impl Hook for ReadOnlyGate {
    fn id(&self) -> &str {
        "read-only-tools"
    }

    fn event(&self) -> HookEvent {
        HookEvent::PreToolUse
    }

    fn matcher(&self) -> Option<&HookMatcher> {
        Some(&self.matcher)
    }

    async fn run(&self, _payload: &HookPayload) -> HookOutcome {
        HookOutcome::Allow
    }
}

#[agentyk::tool(description = "Look up an order by id.")]
async fn lookup_order(order_id: String) -> ToolOutput {
    ToolOutput::text(format!("{order_id}: ready_to_ship"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_path = std::env::temp_dir().join("agentyk-production-agent.jsonl");
    if log_path.exists() {
        std::fs::remove_file(&log_path)?;
    }

    let driver = SimDriver::new([
        SimTurn::tool_call("lookup_order", json!({"order_id": "A-42"})),
        SimTurn::text("Order A-42 is ready to ship."),
        SimTurn::text("I can also help track it after dispatch."),
    ]);
    let agent = Agent::builder()
        .name("support-agent")
        .system_prompt("Answer from tool results; do not invent order status.")
        .model(ModelSpec::llmsim())
        .driver(driver)
        .tool(lookup_order)
        .hook(ReadOnlyGate {
            matcher: HookMatcher {
                tool_name: Some("lookup_order".into()),
                ..HookMatcher::default()
            },
        })
        .build()?;

    let log = Arc::new(JsonlEventLog::new(&log_path)?);
    let mut session = agent.session_with_log(log.clone());
    let first = session.run("What is the status of order A-42?").await?;
    println!("assistant: {}", first.response);

    let second = session.run("What can you do next?").await?;
    println!("assistant: {}", second.response);

    let session_id = session.id();
    drop(session);
    drop(log);

    // A new log handle simulates restarting the host process. Replay restores
    // the complete message history from durable events.
    let reopened = Arc::new(JsonlEventLog::new(&log_path)?);
    let resumed = agent.resume_session(reopened, session_id).await?;
    println!(
        "resumed session {session_id} with {} messages",
        resumed.messages().len()
    );

    std::fs::remove_file(log_path)?;
    Ok(())
}
