//! Per-turn model/reasoning overrides via `Session::run_controlled`.

use std::sync::Arc;

use agentyk::{Agent, ModelSpec, Provider, Result, RunOptions, SimDriver, SimTurn, TurnControls};

#[tokio::test]
async fn run_controlled_overrides_the_model_for_one_turn() -> Result<()> {
    let default_driver = Arc::new(SimDriver::new([SimTurn::text("from default")]));
    let alt_driver = Arc::new(SimDriver::new([SimTurn::text("from alt")]));

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(ForwardingDriver(default_driver)))
        .provider(Provider::new("alt", ForwardingDriver(alt_driver)))
        .build()?;

    // Plain run: the agent's default model, on its own provider.
    let mut session = agent.session();
    let turn = session.run("hi").await?;
    assert_eq!(turn.response, "from default");

    // Controlled run: a per-turn model override picks the other driver,
    // without rebuilding the agent.
    let controls = TurnControls::new().model(ModelSpec::on("alt", "alt-model"));
    let turn = session.run_controlled("hi again", controls).await?;
    assert_eq!(turn.response, "from alt");

    // A subsequent plain run falls back to the agent's default again — the
    // override was per-turn, not sticky. The default driver's script is
    // exhausted (its one line was consumed by the first run), which is
    // itself proof the default (not "alt") provider was routed to.
    let turn = session.run("once more").await?;
    assert_eq!(turn.response, "(llmsim: script exhausted)");
    Ok(())
}

#[tokio::test]
async fn run_with_options_defaults_match_plain_run() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("ok")])))
        .build()?;

    let mut session = agent.session();
    let turn = session
        .run_with_options("hi", RunOptions::default())
        .await?;
    assert_eq!(turn.response, "ok");
    Ok(())
}

/// Shares one `Arc<SimDriver>` so the test can keep a handle on the script
/// while two providers hold independently-scripted drivers.
struct ForwardingDriver(Arc<SimDriver>);

#[async_trait::async_trait]
impl agentyk::ChatDriver for ForwardingDriver {
    async fn complete(&self, request: agentyk::ChatRequest) -> Result<agentyk::ChatResponse> {
        self.0.complete(request).await
    }
}
