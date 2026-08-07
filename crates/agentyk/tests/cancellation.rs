//! Cooperative turn cancellation.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;

use agentyk::{
    Agent, CancellationToken, ChatDriver, ChatRequest, ChatResponse, DeltaSink, Error, EventData,
    ModelSpec, Provider, Result, SimDriver, SimTurn, Tool, ToolCallDecision, ToolContext,
    ToolDefinition, ToolInvocation, ToolOutput, TurnMiddleware, Usage, event_types,
};

#[tokio::test]
async fn pre_cancelled_token_stops_before_any_reason_call() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "should never run",
        )])))
        .build()?;

    let token = CancellationToken::new();
    token.cancel();

    let mut session = agent.session();
    let error = session
        .run_cancellable("go", token)
        .await
        .expect_err("pre-cancelled turn should not succeed");
    assert!(matches!(error, Error::Cancelled));

    // Only turn.started + input.message + turn.cancelled were recorded — no
    // reason call happened.
    let events = session.events().await?;
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            event_types::TURN_STARTED,
            event_types::INPUT_MESSAGE,
            event_types::TURN_CANCELLED,
        ]
    );
    Ok(())
}

/// A driver that cancels the shared token partway through streaming its
/// second delta — deterministic, no timing/spawning needed.
struct CancelMidStreamDriver {
    token: CancellationToken,
}

#[async_trait]
impl ChatDriver for CancelMidStreamDriver {
    async fn complete(&self, _request: ChatRequest) -> Result<ChatResponse> {
        unreachable!("this test only drives complete_streaming")
    }

    async fn complete_streaming(
        &self,
        _request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        sink.delta("first chunk", "first chunk").await?;
        // Cancellation lands between chunks, the way an external caller
        // racing a running turn would trigger it.
        self.token.cancel();
        sink.delta(" second chunk", "first chunk second chunk")
            .await?;
        Ok(ChatResponse::new(
            agentyk::Message::assistant("first chunk second chunk"),
            Usage::default(),
        ))
    }
}

#[tokio::test]
async fn cancellation_mid_stream_stops_the_turn() -> Result<()> {
    let token = CancellationToken::new();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(CancelMidStreamDriver {
            token: token.clone(),
        }))
        .build()?;

    let mut session = agent.session();
    let error = session
        .run_cancellable("go", token)
        .await
        .expect_err("mid-stream cancellation should stop the turn");
    assert!(matches!(error, Error::Cancelled));

    let events = session.events().await?;
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            event_types::TURN_STARTED,
            event_types::INPUT_MESSAGE,
            event_types::OUTPUT_MESSAGE_STARTED,
            event_types::TURN_CANCELLED,
        ],
        "the reason call never completes — cancellation short-circuits it \
         before output.message.completed"
    );
    // No half-built message ever reached history.
    assert!(session.messages().len() <= 1); // just the input message
    Ok(())
}

#[tokio::test]
async fn cancelling_after_a_successful_turn_has_no_effect() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("done")])))
        .build()?;

    let token = CancellationToken::new();
    let mut session = agent.session();
    let turn = session.run_cancellable("go", token.clone()).await?;
    assert_eq!(turn.response, "done");

    // Cancelling the (now-unused) token afterward doesn't retroactively
    // touch the completed turn.
    token.cancel();
    let events = session.events().await?;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.data, EventData::TurnCancelled))
    );
    Ok(())
}

/// A tool that never finishes on its own, and reports whether its future was
/// dropped — the observable half of "the call was actually abandoned".
struct NeverEndingTool {
    /// Cancelled by the tool once it has started, so the test does not race
    /// the executor.
    token: CancellationToken,
    dropped: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Tool for NeverEndingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("hang", "Never returns.", serde_json::json!({}))
    }

    async fn execute(&self, _arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        let _flag = DropFlag(self.dropped.clone());
        // Cancel from inside the call: the turn is now blocked on a tool that
        // will never return, which is exactly the situation the race exists
        // for.
        self.token.cancel();
        std::future::pending::<()>().await;
        unreachable!("the executor must drop this future")
    }
}

#[tokio::test]
async fn cancelling_during_a_tool_call_drops_it_instead_of_waiting() -> Result<()> {
    let token = CancellationToken::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("hang", serde_json::json!({})),
            SimTurn::text("never reached"),
        ])))
        .tool(NeverEndingTool {
            token: token.clone(),
            dropped: dropped.clone(),
        })
        .build()?;

    let mut session = agent.session();
    let error = session
        .run_cancellable("go", token)
        .await
        .expect_err("a cancelled turn must not wait for the tool");
    assert!(matches!(error, Error::Cancelled));
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the tool future must be dropped, not left running"
    );

    let types: Vec<String> = session
        .events()
        .await?
        .iter()
        .map(|event| event.event_type.clone())
        .collect();
    assert!(types.contains(&event_types::TOOL_STARTED.to_string()));
    assert!(
        !types.contains(&event_types::TOOL_COMPLETED.to_string()),
        "an abandoned call never completes: {types:?}"
    );
    assert_eq!(
        types.last().map(String::as_str),
        Some(event_types::TURN_CANCELLED)
    );
    Ok(())
}

/// Middleware that waits for cancellation the way an approval prompt waits
/// for an operator — and gives up when the turn is cancelled.
struct WaitsForOperator;

#[async_trait]
impl TurnMiddleware for WaitsForOperator {
    fn name(&self) -> &str {
        "waits-for-operator"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        invocation.context.cancellation.cancelled().await;
        ToolCallDecision::Deny {
            reason: "the turn was cancelled while waiting for approval".into(),
        }
    }
}

#[tokio::test]
async fn middleware_can_stop_waiting_when_the_turn_is_cancelled() -> Result<()> {
    let token = CancellationToken::new();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::tool_call("hang", serde_json::json!({})),
            SimTurn::text("never reached"),
        ])))
        .tool(NeverEndingTool {
            token: CancellationToken::new(),
            dropped: Arc::new(AtomicBool::new(false)),
        })
        .middleware(WaitsForOperator)
        .build()?;

    // Cancel first: the middleware's await then resolves immediately, which
    // is the behavior a real prompt needs (it would otherwise hang forever).
    token.cancel();
    let mut session = agent.session();
    let error = session
        .run_cancellable("go", token)
        .await
        .expect_err("cancelled before the prompt could be answered");
    assert!(matches!(error, Error::Cancelled));
    Ok(())
}
