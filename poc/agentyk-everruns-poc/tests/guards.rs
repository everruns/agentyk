//! Guardrails as core middleware: mutate a call before it runs, compose
//! several, and let a capability contribute the guard that governs its own
//! tool — the everruns guardrail shapes, expressed with core's
//! `TurnMiddleware` rather than a satellite trait plus a forked act loop.

use std::sync::Arc;

use agentyk::{
    Agent, Capability, EventData, FnTool, ModelSpec, Result, SimDriver, SimTurn, Tool, ToolOutput,
};
use agentyk_core::message::ToolCall;
use agentyk_core::middleware::{ToolCallDecision, ToolInvocation, TurnMiddleware};
use agentyk_everruns_poc::{
    ApprovalDecision, ApprovalMiddleware, Approver, EverrunsExecutor, HintedTool, ToolHints,
};
use async_trait::async_trait;
use serde_json::json;

/// Middleware that redacts a `secret` argument before the tool ever sees it.
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

struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Deny {
            user_message: "denied by policy".into(),
        }
    }
}

/// A tool that echoes the arguments it actually received.
fn echo_tool() -> FnTool {
    FnTool::new(
        "store",
        "Store a value.",
        json!({"type": "object"}),
        |args| async move { ToolOutput::text(args.to_string()) },
    )
}

fn tool_output(events: &[agentyk::Event]) -> Option<String> {
    events.iter().find_map(|e| match &e.data {
        EventData::ToolCompleted { output, .. } => Some(output.clone()),
        _ => None,
    })
}

fn was_denied(events: &[agentyk::Event]) -> bool {
    events
        .iter()
        .any(|e| matches!(&e.data, EventData::ToolDenied { .. }))
}

#[tokio::test]
async fn a_guard_rewrites_a_call_before_it_runs() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("store", json!({"secret": "hunter2"})),
            SimTurn::text("stored"),
        ]))
        .executor(EverrunsExecutor)
        .middleware(RedactSecret)
        .tool(echo_tool())
        .build()?;

    let mut session = agent.session();
    session.run("store my secret").await?;

    let output = tool_output(&session.events().await?).expect("a tool.completed event");
    // The tool saw the redacted args, never the real secret.
    assert!(output.contains("***"));
    assert!(!output.contains("hunter2"));
    Ok(())
}

#[tokio::test]
async fn guards_compose_and_the_first_deny_short_circuits() -> Result<()> {
    // Chain: redact (would rewrite) then approval (denies the destructive
    // tool). The deny wins — the tool never runs.
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("store", json!({"secret": "hunter2"})),
            SimTurn::text("done"),
        ]))
        .executor(EverrunsExecutor)
        .middleware(RedactSecret)
        .middleware(ApprovalMiddleware::new(DenyAll))
        .tool(HintedTool::new(echo_tool(), ToolHints::destructive()))
        .build()?;

    let mut session = agent.session();
    session.run("store").await?;
    let events = session.events().await?;
    assert!(was_denied(&events), "the approval guard should deny");
    let output = tool_output(&events).expect("a tool.completed event");
    assert_eq!(output, "denied by policy");
    Ok(())
}

/// A satellite capability that bundles a (destructive) tool AND provides the
/// guard governing it — the everruns "capability-contributed hook" shape.
struct SecretVault;

impl SecretVault {
    fn guard() -> ApprovalMiddleware {
        ApprovalMiddleware::new(DenyAll)
    }
}

#[async_trait]
impl Capability for SecretVault {
    fn id(&self) -> &str {
        "secret_vault"
    }

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(HintedTool::new(echo_tool(), ToolHints::destructive())) as Arc<dyn Tool>,
        ])
    }
}

#[tokio::test]
async fn a_capability_contributes_its_own_guard() -> Result<()> {
    // The capability provides the tool; its own guard is attached as
    // middleware — both halves of one everruns "capability with a guardrail".
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("store", json!({})),
            SimTurn::text("ok"),
        ]))
        .capability(SecretVault)
        .executor(EverrunsExecutor)
        .middleware(SecretVault::guard())
        .build()?;

    let mut session = agent.session();
    session.run("use it").await?;
    let events = session.events().await?;
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.data, EventData::ToolDenied { name, .. } if name == "store")),
        "the capability's contributed guard should gate its tool"
    );
    Ok(())
}
