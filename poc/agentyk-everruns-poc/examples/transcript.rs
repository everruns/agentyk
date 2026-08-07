//! A runnable end-to-end demo of Everruns-flavored middleware with a
//! guard chain (redaction + hint-based approval) and a `NarrationListener`,
//! driven offline by the scripted `SimDriver`. It prints a transcript rendered
//! entirely from the event stream — including the tool risk-hint (`🔎`/`⚠`) and
//! redaction (`✎`) lines the executor emits as `EventData::Custom` — with no
//! core changes and no API keys.
//!
//! Run it:
//!
//! ```text
//! cargo run -p agentyk-everruns-poc --example transcript
//! ```

use std::sync::Arc;

use agentyk::{
    Agent, FnTool, ModelSpec, Provider, Result, SimDriver, SimToolCall, SimTurn, ToolOutput,
};
use agentyk_core::message::ToolCall;
use agentyk_core::middleware::{ToolCallDecision, ToolInvocation, TurnMiddleware};
use agentyk_everruns_poc::{
    ApprovalDecision, ApprovalMiddleware, Approver, HintedTool, NarrationListener, ToolHints,
};
use async_trait::async_trait;
use serde_json::json;

/// Redacts a `secret` argument before any tool sees it.
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

/// A policy that blocks destructive/open-world tools (what a human-approval
/// prompt would gate, here decided automatically for the demo).
struct BlockRisky;

#[async_trait]
impl Approver for BlockRisky {
    async fn approve(&self, call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Deny {
            user_message: format!("`{}` needs approval — blocked for the demo", call.name),
        }
    }
}

fn echo(name: &'static str) -> FnTool {
    FnTool::new(
        name,
        name,
        json!({"type": "object"}),
        move |args| async move { ToolOutput::text(format!("{name} ran with {args}")) },
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let narration: Arc<NarrationListener> = Arc::default();

    let agent = Agent::builder()
        .name("guarded-agent")
        .system_prompt("You are a careful assistant.")
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            // A batch: one safe read + one destructive delete, dispatched together.
            SimTurn::tool_calls([
                SimToolCall::new("search", json!({"q": "cats"})),
                SimToolCall::new("delete_all", json!({"path": "/"})),
            ]),
            // Then a tool whose secret argument is redacted before it runs.
            SimTurn::tool_call("save_note", json!({"note": "hi", "secret": "p@ssw0rd"})),
            SimTurn::text("All done — one search ran, the delete was blocked, and the secret never reached the tool."),
        ])))
        // Guardrails are ordinary middleware in the canonical engine:
        // redact first, then gate risky tools through the approval policy.
        .middleware(RedactSecret)
        .middleware(ApprovalMiddleware::new(BlockRisky))
        .listener_arc(narration.clone())
        .tool(HintedTool::new(echo("search"), ToolHints::readonly()))
        .tool(HintedTool::new(echo("delete_all"), ToolHints::destructive()))
        .tool(echo("save_note"))
        .build()?;

    let turn = agent
        .run("search for cats, delete everything, and save a note")
        .await?;

    println!("── transcript ─────────────────────────────");
    for line in narration.lines() {
        println!("{line}");
    }
    println!("───────────────────────────────────────────");
    println!("final: {}", turn.response);
    Ok(())
}
