//! The provider seam: what a driver is handed, and when.
//!
//! These live here rather than in core because core has no async runtime —
//! and the interesting behavior is all in the awaiting: credentials are
//! resolved per request, not frozen when the agent was composed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentyk::{
    Agent, BearerAuth, ChatDriver, ChatRequest, ChatResponse, Message, ModelSpec, Provider,
    ProviderAuth, Result, Usage,
};
use async_trait::async_trait;

/// Answers with whatever endpoint the provider resolved, so a test can assert
/// on what the driver was actually handed.
#[derive(Default)]
struct EchoEndpointDriver;

#[async_trait]
impl ChatDriver for EchoEndpointDriver {
    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        let headers = request
            .endpoint
            .headers
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(ChatResponse::new(
            Message::assistant(format!(
                "{} {headers}",
                request.endpoint.base_url.as_deref().unwrap_or("<none>")
            )),
            Usage::default(),
        ))
    }
}

/// A credential that changes every time it is asked for — an expiring token,
/// compressed.
struct RotatingAuth(AtomicUsize);

#[async_trait]
impl ProviderAuth for RotatingAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![("authorization".to_string(), format!("Bearer t{n}"))])
    }
}

#[tokio::test]
async fn the_driver_is_handed_the_endpoint_and_credentials_the_provider_resolved() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::on("gateway", "m"))
        .provider(
            Provider::new("gateway", EchoEndpointDriver)
                .base_url("https://llm.corp.internal/")
                .header("X-Corp-Cost-Center", "research")
                .auth(BearerAuth::new("secret")),
        )
        .build()?;

    let turn = agent.session().run("hi").await?;

    assert_eq!(
        turn.response,
        "https://llm.corp.internal x-corp-cost-center=research authorization=Bearer secret"
    );
    Ok(())
}

#[tokio::test]
async fn credentials_are_resolved_per_turn_so_an_expiring_token_can_refresh() -> Result<()> {
    let agent = Agent::builder()
        .model(ModelSpec::on("gateway", "m"))
        .provider(
            Provider::new("gateway", EchoEndpointDriver)
                .base_url("https://llm.corp.internal")
                .auth_arc(Arc::new(RotatingAuth(AtomicUsize::new(0)))),
        )
        .build()?;

    let mut session = agent.session();
    let first = session.run("hi").await?;
    let second = session.run("hi again").await?;

    assert!(first.response.ends_with("Bearer t0"), "{}", first.response);
    assert!(
        second.response.ends_with("Bearer t1"),
        "{}",
        second.response
    );
    Ok(())
}

#[tokio::test]
async fn one_driver_serves_several_services() -> Result<()> {
    // The point of the split: the same protocol object, two endpoints and two
    // credentials, chosen per turn by the model spec.
    let driver: Arc<dyn ChatDriver> = Arc::new(EchoEndpointDriver);

    let agent = Agent::builder()
        .model(ModelSpec::on("vendor-a", "m"))
        .provider(
            Provider::from_driver("vendor-a", driver.clone())
                .base_url("https://a.example")
                .auth(BearerAuth::new("key-a")),
        )
        .provider(
            Provider::from_driver("vendor-b", driver)
                .base_url("https://b.example")
                .auth(BearerAuth::new("key-b")),
        )
        .build()?;

    let mut session = agent.session();
    let default = session.run("hi").await?;
    let overridden = session
        .run_controlled(
            "hi",
            agentyk::TurnControls::new().model(ModelSpec::on("vendor-b", "m")),
        )
        .await?;

    assert_eq!(
        default.response,
        "https://a.example authorization=Bearer key-a"
    );
    assert_eq!(
        overridden.response,
        "https://b.example authorization=Bearer key-b"
    );
    Ok(())
}
