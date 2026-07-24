//! A runnable end-to-end demo of the satellite: a `EverrunsExecutor` with a
//! guard chain (redaction + hint-based approval) and a `NarrationListener`,
//! driven offline by the scripted `SimDriver`. It prints a transcript rendered
//! entirely from the event stream — no core changes, no API keys.
//!
//! Run it:
//!
//! ```text
//! cargo run -p agentyk-everruns-poc --example transcript
//! ```

use std::sync::Arc;

use agentyk::{Agent, FnTool, ModelSpec, Result, SimDriver, SimToolCall, SimTurn, ToolOutput};
use agentyk_core::message::ToolCall;
use agentyk_core::tool::ToolContext;
use agentyk_everruns_poc::{
    ApprovalDecision, Approver, EverrunsExecutor, GuardOutcome, HintedTool, NarrationListener,
    PreToolGuard, ToolHints,
};
use async_trait::async_trait;
use serde_json::json;

/// Redacts a `secret` argument before any tool sees it.
struct RedactSecret;

#[async_trait]
impl PreToolGuard for RedactSecret {
    async fn inspect(&self, call: &ToolCall, _h: &ToolHints, _c: &ToolContext) -> GuardOutcome {
        if call.arguments.get("secret").is_some() {
            let mut arguments = call.arguments.clone();
            arguments["secret"] = json!("***");
            return GuardOutcome::Rewrite(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments,
            });
        }
        GuardOutcome::Allow
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
        .driver(SimDriver::new([
            // A batch: one safe read + one destructive delete, dispatched together.
            SimTurn {
                text: String::new(),
                tool_calls: vec![
                    SimToolCall { name: "search".into(), arguments: json!({"q": "cats"}) },
                    SimToolCall { name: "delete_all".into(), arguments: json!({"path": "/"}) },
                ],
            },
            // Then a tool whose secret argument is redacted before it runs.
            SimTurn::tool_call("save_note", json!({"note": "hi", "secret": "p@ssw0rd"})),
            SimTurn::text("All done — one search ran, the delete was blocked, and the secret never reached the tool."),
        ]))
        // The satellite executor: a two-guard chain — redact first, then gate
        // risky tools through the approval policy.
        .executor(
            EverrunsExecutor::with_guards(vec![Arc::new(RedactSecret)])
                .guard(ApprovalOf(Arc::new(BlockRisky))),
        )
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

/// Adapts an `Approver` into a `PreToolGuard` (the executor's `::new` does this
/// internally; here we do it explicitly to compose it after the redactor).
struct ApprovalOf(Arc<dyn Approver>);

#[async_trait]
impl PreToolGuard for ApprovalOf {
    async fn inspect(&self, call: &ToolCall, hints: &ToolHints, _c: &ToolContext) -> GuardOutcome {
        if !hints.needs_approval() {
            return GuardOutcome::Allow;
        }
        match self.0.approve(call, hints).await {
            ApprovalDecision::Allow => GuardOutcome::Allow,
            ApprovalDecision::Deny { user_message } => GuardOutcome::Deny { user_message },
        }
    }
}
