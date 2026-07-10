//! Act hooks: pre-tool denial and post-tool output transformation.

use async_trait::async_trait;
use serde_json::json;

use agentyk::{
    Agent, EventData, FnTool, ModelSpec, PostToolExecHook, PreToolUseDecision, PreToolUseHook,
    Result, SimDriver, SimTurn, ToolCall, ToolContext, ToolOutput, event_types,
};

fn echo_tool() -> FnTool {
    FnTool::new(
        "echo",
        "Echo the input.",
        json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        |args| async move { ToolOutput::text(args["text"].as_str().unwrap_or_default()) },
    )
}

struct DenyByName {
    denied: &'static str,
}

#[async_trait]
impl PreToolUseHook for DenyByName {
    async fn before_tool_use(&self, call: &ToolCall, _context: &ToolContext) -> PreToolUseDecision {
        if call.name == self.denied {
            PreToolUseDecision::Deny {
                reason: format!("`{}` is not allowed", call.name),
            }
        } else {
            PreToolUseDecision::Allow
        }
    }
}

#[tokio::test]
async fn denied_tool_never_executes() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "should not run"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .pre_tool_hook(DenyByName { denied: "echo" })
        .build()?;

    let mut session = agent.session();
    session.run("go").await?;

    let events = session.events().await?;
    let denial = events
        .iter()
        .find(|e| matches!(e.data, EventData::ToolDenied { .. }))
        .expect("a tool.denied event was recorded");
    assert_eq!(denial.event_type, event_types::TOOL_DENIED);
    if let EventData::ToolDenied {
        name,
        reason,
        call_id,
    } = &denial.data
    {
        assert_eq!(name, "echo");
        assert!(reason.contains("not allowed"));
        assert!(!call_id.is_empty());
    }

    // The model saw the denial as an error result, not the tool's real output.
    let completed = events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolCompleted {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(completed.0.contains("not allowed"));
    assert!(completed.1);
    Ok(())
}

#[tokio::test]
async fn allowed_tool_runs_normally_with_hook_present() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "hi"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .pre_tool_hook(DenyByName {
            denied: "not-this-one",
        })
        .build()?;

    let turn = agent.run("go").await?;
    assert_eq!(turn.response, "done");
    assert_eq!(turn.tool_calls, 1);
    Ok(())
}

struct Truncate {
    max_len: usize,
}

#[async_trait]
impl PostToolExecHook for Truncate {
    async fn after_tool_exec(
        &self,
        _call: &ToolCall,
        mut output: ToolOutput,
        _context: &ToolContext,
    ) -> ToolOutput {
        if output.content.len() > self.max_len {
            output.content.truncate(self.max_len);
            output.content.push_str("...[truncated]");
        }
        output
    }
}

#[tokio::test]
async fn post_hook_transforms_tool_output() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "0123456789"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .post_tool_hook(Truncate { max_len: 4 })
        .build()?;

    let mut session = agent.session();
    session.run("go").await?;

    let events = session.events().await?;
    let output = events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(output, "0123...[truncated]");
    Ok(())
}

struct RecordCall {
    name: &'static str,
}

#[async_trait]
impl PostToolExecHook for RecordCall {
    async fn after_tool_exec(
        &self,
        _call: &ToolCall,
        mut output: ToolOutput,
        _context: &ToolContext,
    ) -> ToolOutput {
        output.content = format!("{}>{}", self.name, output.content);
        output
    }
}

#[tokio::test]
async fn post_hooks_run_in_registration_order() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "x"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .post_tool_hook(RecordCall { name: "first" })
        .post_tool_hook(RecordCall { name: "second" })
        .build()?;

    let mut session = agent.session();
    session.run("go").await?;

    let events = session.events().await?;
    let output = events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    // "x" -> first wraps -> "first>x" -> second wraps -> "second>first>x"
    assert_eq!(output, "second>first>x");
    Ok(())
}
