//! Multi-actor sessions: host-owned membership, shared history, per-turn agents.

use std::sync::Arc;

use agentyk::{
    Agent, ChatDriver, ChatRequest, ChatResponse, ModelSpec, Provider, Result, SimDriver, SimTurn,
};

struct SharedSim(Arc<SimDriver>);

#[async_trait::async_trait]
impl ChatDriver for SharedSim {
    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.0.complete(request).await
    }
}

#[tokio::test]
async fn addressed_agent_overlays_behavior_on_the_hosts_harness_and_history() -> Result<()> {
    let host_driver = Arc::new(SimDriver::new([
        SimTurn::text("host answer"),
        SimTurn::text("guest answer"),
    ]));
    let guest_driver = Arc::new(SimDriver::new([SimTurn::text(
        "must not run on the guest harness",
    )]));
    let host = Agent::builder()
        .name("host")
        .system_prompt("You are the host.")
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SharedSim(host_driver.clone())))
        .build()?;
    let guest = Agent::builder()
        .name("reviewer")
        .system_prompt("You are the reviewer.")
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SharedSim(guest_driver.clone())))
        .build()?;

    let mut session = host.session();
    session.run("draft this").await?;
    let turn = session.run_with_agent(&guest, "review it").await?;

    assert_eq!(turn.response, "guest answer");
    assert_eq!(session.messages().len(), 4);
    assert!(guest_driver.recorded_requests().is_empty());
    let requests = host_driver.recorded_requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[1];
    assert_eq!(
        request.system_prompt.as_deref(),
        Some("You are the reviewer.")
    );
    assert_eq!(
        request
            .messages
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>(),
        ["draft this", "host answer", "review it"]
    );
    Ok(())
}
