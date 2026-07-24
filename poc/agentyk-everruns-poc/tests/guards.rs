//! Pre-tool guards: mutate a call before it runs, compose several guards, and
//! let a capability contribute the guard that governs its own tool — the
//! everruns guardrail shapes core's `PreToolUseHook` can't express, all in a
//! satellite over agentyk-core's seams.

use std::sync::Arc;

use agentyk::{
    Agent, Capability, EventData, FnTool, ModelSpec, Result, SimDriver, SimTurn, Tool, ToolOutput,
};
use agentyk_core::message::ToolCall;
use agentyk_core::tool::ToolContext;
use agentyk_everruns_poc::{
    ApprovalDecision, Approver, EverrunsExecutor, GuardOutcome, HintedTool, PreToolGuard, ToolHints,
};
use async_trait::async_trait;
use serde_json::json;

/// A guard that redacts a `secret` argument before the tool ever sees it.
struct RedactSecret;

#[async_trait]
impl PreToolGuard for RedactSecret {
    async fn inspect(
        &self,
        call: &ToolCall,
        _hints: &ToolHints,
        _ctx: &ToolContext,
    ) -> GuardOutcome {
        if call.arguments.get("secret").is_some() {
            let mut arguments = call.arguments.clone();
            arguments["secret"] = json!("***");
            GuardOutcome::Rewrite(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments,
            })
        } else {
            GuardOutcome::Allow
        }
    }
}

/// A guard that consults an approver for tools whose hints need approval —
/// the same policy `EverrunsExecutor::new` applies, usable in an explicit chain.
struct ApprovalGuard(Arc<dyn Approver>);

#[async_trait]
impl PreToolGuard for ApprovalGuard {
    async fn inspect(
        &self,
        call: &ToolCall,
        hints: &ToolHints,
        _ctx: &ToolContext,
    ) -> GuardOutcome {
        if !hints.needs_approval() {
            return GuardOutcome::Allow;
        }
        match self.0.approve(call, hints).await {
            ApprovalDecision::Allow => GuardOutcome::Allow,
            ApprovalDecision::Deny { user_message } => GuardOutcome::Deny { user_message },
        }
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
        .executor(EverrunsExecutor::with_guards(vec![Arc::new(RedactSecret)]))
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
        .executor(
            EverrunsExecutor::with_guards(vec![Arc::new(RedactSecret)])
                .guard(ApprovalGuard(Arc::new(DenyAll))),
        )
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
    fn guard() -> ApprovalGuard {
        ApprovalGuard(Arc::new(DenyAll))
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
    // The capability provides the tool; its own guard is attached to the
    // executor — both halves of one everruns "capability with a guardrail".
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("store", json!({})),
            SimTurn::text("ok"),
        ]))
        .capability(SecretVault)
        .executor(EverrunsExecutor::with_guards(vec![Arc::new(
            SecretVault::guard(),
        )]))
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
