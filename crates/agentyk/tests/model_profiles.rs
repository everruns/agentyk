//! Model profiles: catching an unsupported request at composition time.

use agentyk::{
    Agent, Error, InMemoryModelCatalog, ModelCatalog, ModelProfile, ModelSpec, Provider,
    ProviderId, Result, SimDriver, SimTurn,
};

fn catalog() -> InMemoryModelCatalog {
    InMemoryModelCatalog::new()
        .with(
            ProviderId::llmsim(),
            "llmsim",
            ModelProfile::new()
                .context_window(8_000)
                .reasoning_efforts(["low", "high"]),
        )
        .with(
            ProviderId::anthropic(),
            "claude-thinker",
            ModelProfile::new().thinking(true),
        )
}

fn builder(model: ModelSpec) -> agentyk::AgentBuilder {
    Agent::builder()
        .model(model)
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
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
        .model(ModelSpec::on(ProviderId::llmsim(), "some-new-model").reasoning_effort("whatever"))
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .model_catalog(catalog())
        .build()?;
    assert!(agent.model_profile().is_none());
    Ok(())
}

#[test]
fn without_a_catalog_nothing_is_validated() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::llmsim().reasoning_effort("ultra"))
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
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
        fn profile(&self, _provider: &ProviderId, _model: &str) -> Option<ModelProfile> {
            Some(ModelProfile::new().reasoning_efforts(["high"]))
        }
    }

    let rejected = Agent::builder()
        .model(ModelSpec::llmsim().reasoning_effort("low"))
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .model_catalog(EverythingIsHigh)
        .build();
    assert!(rejected.is_err());
}

#[test]
fn a_stated_window_and_output_ceiling_imply_the_input_budget() -> Result<()> {
    let catalog = InMemoryModelCatalog::new().with(
        ProviderId::llmsim(),
        "llmsim",
        ModelProfile::new()
            .context_window(200_000)
            .max_output_tokens(8_000),
    );
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .model_catalog(catalog)
        .build()?;
    assert_eq!(agent.definition().context_token_limit, Some(192_000));
    Ok(())
}

#[test]
fn an_explicit_limit_wins_over_the_catalog() -> Result<()> {
    let catalog = InMemoryModelCatalog::new().with(
        ProviderId::llmsim(),
        "llmsim",
        ModelProfile::new()
            .context_window(200_000)
            .max_output_tokens(8_000),
    );
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .model_catalog(catalog)
        .context_token_limit(50_000)
        .build()?;
    assert_eq!(agent.definition().context_token_limit, Some(50_000));
    Ok(())
}

#[test]
fn a_window_alone_implies_nothing() -> Result<()> {
    // Without an output ceiling there is no honest input budget — reserving
    // room would mean inventing a number the host never stated.
    let catalog = InMemoryModelCatalog::new().with(
        ProviderId::llmsim(),
        "llmsim",
        ModelProfile::new().context_window(200_000),
    );
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("hi")])))
        .model_catalog(catalog)
        .build()?;
    assert_eq!(agent.definition().context_token_limit, None);
    Ok(())
}
