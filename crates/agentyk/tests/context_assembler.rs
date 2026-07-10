//! `ContextAssembler`: transforms replayed history into what's actually
//! sent to the model this turn.

use std::sync::Arc;

use async_trait::async_trait;

use agentyk::{Agent, ContextAssembler, Message, ModelSpec, Result, SessionId, SimDriver, SimTurn};

/// Keeps only the most recent `keep` messages — a minimal stand-in for
/// real compaction/trimming.
struct KeepLast {
    keep: usize,
}

#[async_trait]
impl ContextAssembler for KeepLast {
    async fn assemble(&self, _session_id: SessionId, messages: &[Message]) -> Vec<Message> {
        let start = messages.len().saturating_sub(self.keep);
        messages[start..].to_vec()
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
