//! Model profiles: catching an unsupported request at composition time.

use agentyk::{
    Agent, DriverId, Error, InMemoryModelCatalog, ModelCatalog, ModelProfile, ModelSpec, Result,
    SimDriver, SimTurn,
};

fn catalog() -> InMemoryModelCatalog {
    InMemoryModelCatalog::new()
        .with(
            DriverId::llmsim(),
            "llmsim",
            ModelProfile::new()
                .context_window(8_000)
                .reasoning_efforts(["low", "high"]),
        )
        .with(
            DriverId::anthropic(),
            "claude-thinker",
            ModelProfile::new().thinking(true),
        )
}

fn builder(model: ModelSpec) -> agentyk::AgentBuilder {
    Agent::builder()
        .model(model)
        .driver(SimDriver::new([SimTurn::text("hi")]))
        .model_catalog(catalog())
}

#[test]
fn a_supported_effort_builds() -> Result<()> {
    let agent = builder(ModelSpec::llmsim().reasoning_effort("high")).build()?;
    assert_eq!(
        agent.model_profile().and_then(|p| p.context_window),
        Some(8_000),
        "the profile is readable afterwards, for budget and UI decisions"
    );
    Ok(())
}

#[test]
fn an_unsupported_effort_fails_the_build_and_names_the_alternatives() {
    let error = builder(ModelSpec::llmsim().reasoning_effort("ultra"))
        .build()
        .expect_err("ultra is not offered");
    let message = error.to_string();
    assert!(matches!(error, Error::InvalidAgent(_)), "{message}");
    assert!(message.contains("ultra"), "{message}");
    assert!(message.contains("low, high"), "{message}");
}

#[test]
fn a_thinking_budget_on_a_non_thinking_model_fails_the_build() {
    let error = builder(ModelSpec::llmsim().thinking_budget(4_000))
        .build()
        .expect_err("llmsim does not think");
    assert!(error.to_string().contains("extended thinking"), "{error}");
}

#[test]
fn an_unknown_model_is_passed_through_untouched() -> Result<()> {
    // The whole point of a catalog being partial: describing some models must
    // never mean rejecting the rest.
    let agent = Agent::builder()
        .model(ModelSpec::new(DriverId::llmsim(), "some-new-model").reasoning_effort("whatever"))
        .driver(SimDriver::new([SimTurn::text("hi")]))
        .model_catalog(catalog())
        .build()?;
    assert!(agent.model_profile().is_none());
    Ok(())
}

#[test]
fn without_a_catalog_nothing_is_validated() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim().reasoning_effort("ultra"))
        .driver(SimDriver::new([SimTurn::text("hi")]))
        .build()?;
    assert!(agent.model_profile().is_none());
    Ok(())
}

#[tokio::test]
async fn a_validated_agent_still_runs_its_turn() -> Result<()> {
    let mut session = builder(ModelSpec::llmsim().reasoning_effort("low"))
        .build()?
        .session();
    assert_eq!(session.run("go").await?.response, "hi");
    Ok(())
}

#[test]
fn a_catalog_can_be_anything_that_answers_the_question() {
    // The seam, not the bundled map: a host with its own source implements
    // the trait over it.
    struct EverythingIsHigh;

    impl ModelCatalog for EverythingIsHigh {
        fn profile(&self, _driver: &DriverId, _model: &str) -> Option<ModelProfile> {
            Some(ModelProfile::new().reasoning_efforts(["high"]))
        }
    }

    let rejected = Agent::builder()
        .model(ModelSpec::llmsim().reasoning_effort("low"))
        .driver(SimDriver::new([SimTurn::text("hi")]))
        .model_catalog(EverythingIsHigh)
        .build();
    assert!(rejected.is_err());
}
