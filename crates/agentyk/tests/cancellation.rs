//! Cooperative turn cancellation.

use async_trait::async_trait;

use agentyk::{
    Agent, CancellationToken, ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId, Error,
    EventData, ModelSpec, Result, SimDriver, SimTurn, Usage, event_types,
};

#[tokio::test]
async fn pre_cancelled_token_stops_before_any_reason_call() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("should never run")]))
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
    fn id(&self) -> DriverId {
        DriverId::llmsim()
    }

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
        .driver(CancelMidStreamDriver {
            token: token.clone(),
        })
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
        .driver(SimDriver::new([SimTurn::text("done")]))
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
