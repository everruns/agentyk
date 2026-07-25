//! Turn middleware: denial, rewriting, and output transformation, end to end
//! through a real turn.

use async_trait::async_trait;
use serde_json::json;

use agentyk::{
    Agent, EventData, FnTool, ModelSpec, Result, SimDriver, SimTurn, ToolCallDecision,
    ToolInvocation, ToolOutput, TurnMiddleware, event_types,
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
impl TurnMiddleware for DenyByName {
    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if invocation.call.name == self.denied {
            ToolCallDecision::Deny {
                reason: format!("`{}` is not allowed", invocation.call.name),
            }
        } else {
            ToolCallDecision::Proceed
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
        .middleware(DenyByName { denied: "echo" })
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
async fn allowed_tool_runs_normally_with_middleware_present() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "hi"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(DenyByName {
            denied: "not-this-one",
        })
        .build()?;

    let turn = agent.run("go").await?;
    assert_eq!(turn.response, "done");
    assert_eq!(turn.tool_calls, 1);
    Ok(())
}

/// Redacts a secret argument before the tool ever sees it — the everruns
/// mutation shape, which used to require forking the whole executor loop.
struct RedactSecret;

#[async_trait]
impl TurnMiddleware for RedactSecret {
    fn name(&self) -> &str {
        "redact-secret"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if invocation
            .call
            .arguments
            .get("text")
            .and_then(|t| t.as_str())
            == Some("p@ssw0rd")
        {
            let mut call = invocation.call.clone();
            call.arguments["text"] = json!("[redacted]");
            return ToolCallDecision::Rewrite(call);
        }
        ToolCallDecision::Proceed
    }
}

#[tokio::test]
async fn a_rewrite_reaches_the_tool_and_is_recorded() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "p@ssw0rd"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(RedactSecret)
        .build()?;

    let mut session = agent.session();
    session.run("go").await?;
    let events = session.events().await?;

    // The tool ran on the rewritten arguments — the secret never reached it.
    let output = events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(output, "[redacted]");

    // The mutation is visible rather than silent...
    let rewritten = events
        .iter()
        .find(|e| matches!(e.data, EventData::ToolRewritten { .. }))
        .expect("a tool.rewritten event was recorded");
    assert_eq!(rewritten.event_type, event_types::TOOL_REWRITTEN);

    // ...and `tool.started` announces the call as it actually ran, so no
    // observer of the event stream ever sees the secret.
    let started = events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolStarted { call } => Some(call.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(started.arguments["text"], json!("[redacted]"));

    // The act-phase events carry only the rewrite. The *reason*-phase event
    // still holds the model's original output, and deliberately so: a tool
    // middleware cannot un-say what the model already generated. Redacting
    // that would need a reason-phase interception point (everruns'
    // `output.message.replaced`), which agentyk does not have — so the
    // guarantee is "the secret never reaches the tool or the act events",
    // not "the secret never exists".
    let carrying_the_secret: Vec<&str> = events
        .iter()
        .filter(|e| serde_json::to_string(&e.data).unwrap().contains("p@ssw0rd"))
        .map(|e| e.event_type.as_str())
        .collect();
    assert_eq!(
        carrying_the_secret,
        vec![event_types::OUTPUT_MESSAGE_COMPLETED]
    );
    Ok(())
}

#[tokio::test]
async fn a_rewrite_feeds_the_next_middleware_and_a_later_deny_still_wins() -> Result<()> {
    struct DenyRedacted;

    #[async_trait]
    impl TurnMiddleware for DenyRedacted {
        async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
            if invocation.call.arguments["text"] == json!("[redacted]") {
                return ToolCallDecision::Deny {
                    reason: "redacted calls are blocked".into(),
                };
            }
            ToolCallDecision::Proceed
        }
    }

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "p@ssw0rd"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(RedactSecret)
        .middleware(DenyRedacted)
        .build()?;

    let mut session = agent.session();
    session.run("go").await?;
    let events = session.events().await?;

    assert!(
        events
            .iter()
            .any(|e| matches!(e.data, EventData::ToolDenied { .. })),
        "the second middleware saw the rewrite and denied it"
    );
    Ok(())
}

struct Truncate {
    max_len: usize,
}

#[async_trait]
impl TurnMiddleware for Truncate {
    async fn after_tool(&self, _invocation: &ToolInvocation<'_>, output: ToolOutput) -> ToolOutput {
        if output.content.len() > self.max_len {
            let mut content = output.content;
            content.truncate(self.max_len);
            content.push_str("...[truncated]");
            return ToolOutput::new(content, output.is_error);
        }
        output
    }
}

#[tokio::test]
async fn after_tool_transforms_tool_output() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "0123456789"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(Truncate { max_len: 4 })
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
    label: &'static str,
}

#[async_trait]
impl TurnMiddleware for RecordCall {
    async fn after_tool(&self, _invocation: &ToolInvocation<'_>, output: ToolOutput) -> ToolOutput {
        ToolOutput::new(
            format!("{}>{}", self.label, output.content),
            output.is_error,
        )
    }
}

#[tokio::test]
async fn after_tool_runs_in_attachment_order() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "x"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(RecordCall { label: "first" })
        .middleware(RecordCall { label: "second" })
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

/// A denied call never reaches `after_tool` — otherwise a transform would
/// rewrite the denial reason the model is supposed to see.
#[tokio::test]
async fn after_tool_does_not_run_for_a_denied_call() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("echo", json!({"text": "x"})),
            SimTurn::text("done"),
        ]))
        .tool(echo_tool())
        .middleware(DenyByName { denied: "echo" })
        .middleware(RecordCall { label: "after" })
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
    assert!(!output.starts_with("after>"), "got {output}");
    assert!(output.contains("not allowed"));
    Ok(())
}
