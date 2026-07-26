//! `ContextAssembler`: transforms replayed history into what's actually
//! sent to the model this turn.

use std::sync::Arc;

use async_trait::async_trait;

use agentyk::{
    Agent, ContextAssembler, ContextAssembly, ContextEvent, ContextRequest, ContextSource,
    ModelSpec, Result, SimDriver, SimTurn,
};

/// Keeps only the most recent `keep` messages — a minimal stand-in for
/// real compaction/trimming.
struct KeepLast {
    keep: usize,
}

#[async_trait]
impl ContextAssembler for KeepLast {
    async fn assemble(&self, request: ContextRequest<'_>) -> Result<ContextAssembly> {
        let observed_head = request.events.head(request.point.session_id).await?;
        let messages = request.messages;
        let start = messages.len().saturating_sub(self.keep);
        let mut assembly = ContextAssembly::new(messages[start..].to_vec());
        assembly.estimated_tokens = Some(assembly.messages.len() * 4);
        assembly.sources.push(ContextSource {
            kind: "recent_history".into(),
            from: None,
            through: Some(request.point),
        });
        assembly.events.push(ContextEvent {
            event_type: "context.compacted".into(),
            payload: serde_json::json!({
                "through": request.point.sequence,
                "iteration": request.iteration,
                "token_limit": request.token_limit,
                "model": request.model.model,
                "observed_head": observed_head.sequence,
            }),
        });
        Ok(assembly)
    }
}

#[tokio::test]
async fn default_passthrough_sends_full_history() -> Result<()> {
    let sim = Arc::new(SimDriver::new([
        SimTurn::text("first"),
        SimTurn::text("second"),
    ]));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(ForwardingDriver(sim.clone()))
        .build()?;

    let mut session = agent.session();
    session.run("one").await?;
    session.run("two").await?;

    // By the second turn, the full 3-message history (user/assistant/user)
    // is what got sent — no trimming happened.
    let requests = sim.recorded_requests();
    assert_eq!(requests[1].messages.len(), 3);
    Ok(())
}

#[tokio::test]
async fn custom_assembler_trims_what_is_sent_without_touching_the_log() -> Result<()> {
    let sim = Arc::new(SimDriver::new([
        SimTurn::text("first"),
        SimTurn::text("second"),
        SimTurn::text("third"),
    ]));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(ForwardingDriver(sim.clone()))
        .context_assembler(KeepLast { keep: 1 })
        .context_token_limit(128)
        .build()?;

    let mut session = agent.session();
    session.run("one").await?;
    session.run("two").await?;
    session.run("three").await?;

    // Only the single most recent message reached the driver each time...
    let requests = sim.recorded_requests();
    assert!(requests.iter().all(|r| r.messages.len() == 1));

    // ...but the full, untrimmed history is still what's in the log —
    // trimming is a per-turn view, not a mutation of what's recorded.
    assert_eq!(session.messages().len(), 6); // 3 user + 3 assistant
    let context_events = session
        .events()
        .await?
        .into_iter()
        .filter(|event| event.event_type == "context.assembled")
        .count();
    assert_eq!(context_events, 3);
    let compacted_events = session
        .events()
        .await?
        .into_iter()
        .filter(|event| event.event_type == "context.compacted")
        .count();
    assert_eq!(compacted_events, 3);
    Ok(())
}

/// Wraps an `Arc<SimDriver>` so a test can keep a handle for assertions.
struct ForwardingDriver(Arc<SimDriver>);

#[async_trait]
impl agentyk::ChatDriver for ForwardingDriver {
    fn id(&self) -> agentyk::DriverId {
        self.0.id()
    }

    async fn complete(&self, request: agentyk::ChatRequest) -> Result<agentyk::ChatResponse> {
        self.0.complete(request).await
    }
}
