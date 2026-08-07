//! End-to-end proof: engine middleware gates tools by their
//! `metadata` hints, driving a real agent through the framework harness — all
//! over agentyk-core's public seams, no core change.

use agentyk::{
    Agent, EventData, FnTool, ModelSpec, Provider, Result, SimDriver, SimTurn, ToolOutput,
};
use agentyk_core::message::ToolCall;
use agentyk_everruns_poc::{ApprovalDecision, ApprovalMiddleware, Approver, HintedTool, ToolHints};
use async_trait::async_trait;
use serde_json::json;

/// An approver that blocks everything with a user-facing message.
struct DenyAll(&'static str);

#[async_trait]
impl Approver for DenyAll {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Deny {
            user_message: self.0.to_string(),
        }
    }
}

fn delete_tool() -> FnTool {
    FnTool::new(
        "delete",
        "Delete a file.",
        json!({"type": "object"}),
        |_args| async move { ToolOutput::text("deleted for real") },
    )
}

/// Build an agent with `delete` tagged `destructive` so engine middleware
/// gates it.
fn agent_with(approver: impl Approver + 'static, hints: ToolHints) -> Result<Agent> {
    Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("delete", json!({})),
            SimTurn::text("done"),
        ])))
        .middleware(ApprovalMiddleware::new(approver))
        .tool(HintedTool::new(delete_tool(), hints))
        .build()
}

fn tool_result(events: &[agentyk::Event]) -> Option<(String, bool)> {
    events.iter().find_map(|e| match &e.data {
        EventData::ToolCompleted {
            output, is_error, ..
        } => Some((output.clone(), *is_error)),
        _ => None,
    })
}

fn was_denied(events: &[agentyk::Event]) -> bool {
    events
        .iter()
        .any(|e| matches!(&e.data, EventData::ToolDenied { .. }))
}

#[tokio::test]
async fn destructive_tool_is_blocked_by_the_approver() -> Result<()> {
    let agent = agent_with(
        DenyAll("blocked: needs human approval"),
        ToolHints::destructive(),
    )?;
    let mut session = agent.session();
    let turn = session.run("delete it").await?;

    // The turn still completes (the model recovers from the error result)...
    assert_eq!(turn.response, "done");

    let events = session.events().await?;
    // ...but the tool never ran: a tool.denied was recorded and the result
    // the model saw is the approver's user_message, as an error.
    assert!(was_denied(&events), "expected a tool.denied event");
    let (output, is_error) = tool_result(&events).expect("a tool.completed event");
    assert!(is_error);
    assert_eq!(output, "blocked: needs human approval");
    assert_ne!(
        output, "deleted for real",
        "the tool body must not have run"
    );
    Ok(())
}

#[tokio::test]
async fn approved_destructive_tool_runs() -> Result<()> {
    let agent = agent_with(agentyk_everruns_poc::AllowAll, ToolHints::destructive())?;
    let mut session = agent.session();
    session.run("delete it").await?;

    let events = session.events().await?;
    assert!(!was_denied(&events));
    let (output, is_error) = tool_result(&events).expect("a tool.completed event");
    assert!(!is_error);
    assert_eq!(output, "deleted for real");
    Ok(())
}

#[tokio::test]
async fn readonly_tool_bypasses_approval_entirely() -> Result<()> {
    // Even with a deny-all approver, a readonly tool is never gated.
    let agent = agent_with(DenyAll("should never be consulted"), ToolHints::readonly())?;
    let mut session = agent.session();
    session.run("read it").await?;

    let events = session.events().await?;
    assert!(!was_denied(&events));
    let (output, _) = tool_result(&events).expect("a tool.completed event");
    assert_eq!(output, "deleted for real");
    Ok(())
}
