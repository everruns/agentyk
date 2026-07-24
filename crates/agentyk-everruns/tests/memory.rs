//! `MemoryAssembler` over the `ContextAssembler` seam: everruns-style memory
//! injection + compaction land in what the model receives, with no core change.
//! Proven by inspecting the driver's recorded requests.

use std::sync::Arc;

use agentyk::{
    Agent, ChatDriver, ChatRequest, ChatResponse, DriverId, ModelSpec, Result, SimDriver, SimTurn,
};
use agentyk_everruns::MemoryAssembler;
use async_trait::async_trait;

/// Wraps an `Arc<SimDriver>` so the test keeps a handle to inspect what was
/// actually sent (the `context_assembler` test pattern from the framework).
struct ForwardingDriver(Arc<SimDriver>);

#[async_trait]
impl ChatDriver for ForwardingDriver {
    fn id(&self) -> DriverId {
        self.0.id()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.0.complete(request).await
    }
}

#[tokio::test]
async fn memory_note_is_prepended_to_what_the_model_sees() -> Result<()> {
    let sim = Arc::new(SimDriver::new([SimTurn::text("ack")]));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(ForwardingDriver(sim.clone()))
        .context_assembler(MemoryAssembler::new("user prefers metric units"))
        .build()?;

    agent.run("convert 5 miles").await?;

    let requests = sim.recorded_requests();
    let sent = &requests[0].messages;
    // The memory note leads, ahead of the user's message — and it never
    // required a core field: it rides the ContextAssembler seam.
    assert_eq!(sent[0].text(), "(memory) user prefers metric units");
    assert_eq!(sent[1].text(), "convert 5 miles");
    Ok(())
}

#[tokio::test]
async fn keep_last_compacts_history_yet_the_note_and_log_survive() -> Result<()> {
    let sim = Arc::new(SimDriver::new([
        SimTurn::text("first"),
        SimTurn::text("second"),
        SimTurn::text("third"),
    ]));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(ForwardingDriver(sim.clone()))
        .context_assembler(MemoryAssembler::new("remember: keep it short").keep_last(1))
        .build()?;

    let mut session = agent.session();
    session.run("one").await?;
    session.run("two").await?;
    session.run("three").await?;

    // Every request carried exactly the memory note + the single most recent
    // history message — compaction happened in the assembled view.
    let requests = sim.recorded_requests();
    assert!(requests.iter().all(|r| r.messages.len() == 2));
    assert!(
        requests
            .iter()
            .all(|r| r.messages[0].text() == "(memory) remember: keep it short")
    );

    // ...but the full, untrimmed history is still in the log.
    assert_eq!(session.messages().len(), 6); // 3 user + 3 assistant
    Ok(())
}
