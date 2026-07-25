//! The canonical engine prepares a whole tool batch and gates each call by
//! its own hints. The in-process host executes that batch sequentially; a
//! durable host may dispatch it concurrently without replacing the engine.

use agentyk::{
    Agent, EventData, FnTool, ModelSpec, Result, SimDriver, SimToolCall, SimTurn, ToolOutput,
};
use agentyk_core::message::ToolCall;
use agentyk_everruns_poc::{ApprovalDecision, ApprovalMiddleware, Approver, HintedTool, ToolHints};
use async_trait::async_trait;
use serde_json::json;

struct DenyDestructive;

#[async_trait]
impl Approver for DenyDestructive {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Deny {
            user_message: "destructive tools need approval".into(),
        }
    }
}

fn tool(name: &'static str, output: &'static str) -> FnTool {
    FnTool::new(
        name,
        name,
        json!({"type": "object"}),
        move |_args| async move { ToolOutput::text(output) },
    )
}

/// One assistant turn that requests two tools at once.
fn two_call_turn() -> SimTurn {
    SimTurn::tool_calls([
        SimToolCall::new("safe_read", json!({})),
        SimToolCall::new("danger_delete", json!({})),
    ])
}

#[tokio::test]
async fn a_batch_is_dispatched_and_gated_per_call() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([two_call_turn(), SimTurn::text("done")]))
        .middleware(ApprovalMiddleware::new(DenyDestructive))
        .tool(HintedTool::new(
            tool("safe_read", "file contents"),
            ToolHints::readonly(),
        ))
        .tool(HintedTool::new(
            tool("danger_delete", "deleted"),
            ToolHints::destructive(),
        ))
        .build()?;

    let mut session = agent.session();
    let turn = session.run("do both").await?;
    assert_eq!(turn.response, "done");
    assert_eq!(turn.tool_calls, 2, "both calls in the batch resolved");

    let events = session.events().await?;
    let completed: Vec<(String, String, bool)> = events
        .iter()
        .filter_map(|e| match &e.data {
            EventData::ToolCompleted {
                name,
                output,
                is_error,
                ..
            } => Some((name.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect();

    // The readonly call ran; the destructive call was gated with the
    // approver's message — both in the one batch.
    assert!(completed.contains(&("safe_read".into(), "file contents".into(), false)));
    assert!(completed.contains(&(
        "danger_delete".into(),
        "destructive tools need approval".into(),
        true
    )));
    assert!(
        events.iter().any(
            |e| matches!(&e.data, EventData::ToolDenied { name, .. } if name == "danger_delete")
        )
    );
    Ok(())
}
