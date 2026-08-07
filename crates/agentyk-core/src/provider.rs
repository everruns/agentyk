//! Providers — the service seam, layered over the protocol seam.
//!
//! A [`ChatDriver`] is a *wire protocol*: how a request body is built, how a
//! response is read, how a stream folds. A [`Provider`] is a *service* that
//! speaks one: OpenAI, OpenRouter, a corporate gateway, a local runtime. The
//! provider owns what the protocol does not — the endpoint, the credentials,
//! any headers the service demands — and holds the driver it speaks through.
//!
//! Keeping them apart is what lets one protocol serve many services (the
//! OpenAI Chat Completions shape is spoken by half a dozen vendors) and one
//! service switch protocols without touching call sites. It also puts every
//! credential in a runtime object that was never serializable, which is why
//! [`ModelSpec`](crate::driver::ModelSpec) can be plain, loggable config: it
//! names a provider, it does not carry a key.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::driver::{ChatDriver, ChatRequest, ChatResponse, DeltaSink};
use crate::error::Result;

/// Names a service, e.g. `"openai"`, `"anthropic"`, `"llmsim"`. An open
/// string (not a closed enum) so embedders can name their own — a gateway, a
/// tenant-specific account, a local runtime.
///
/// This is the routing identity: a [`ModelSpec`](crate::driver::ModelSpec)
/// naming it resolves to the [`Provider`] registered under it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Name a service. Use the constructors below for the ones agentyk
    /// bundles a provider for; this is for your own.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written in a [`ModelSpec`](crate::driver::ModelSpec) and in
    /// config.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `"openai"` — OpenAI itself.
    pub fn openai() -> Self {
        Self::new("openai")
    }

    /// `"anthropic"` — Anthropic itself.
    pub fn anthropic() -> Self {
        Self::new("anthropic")
    }

    /// `"llmsim"` — the scripted offline simulator, for tests and examples.
    pub fn llmsim() -> Self {
        Self::new("llmsim")
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ProviderId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// Supplies the credential headers a service authenticates with, **asked once
/// per request** rather than read once at construction.
///
/// That is the whole reason this is a trait and not a `String` field: a
/// subscription's OAuth access token expires mid-session, and a credential
/// frozen when the agent was built cannot be refreshed. The same shape as
/// `McpAuthProvider` on the MCP side, for the same reason.
///
/// Services whose signature covers the request body rather than just its
/// headers (AWS SigV4) do not fit this trait; they belong in a driver-level
/// signing hook, which does not exist yet.
#[async_trait]
pub trait ProviderAuth: Send + Sync {
    /// The headers to attach to this request. Called before every completion,
    /// so an implementation is free to refresh an expiring token here.
    async fn headers(&self) -> Result<Vec<(String, String)>>;
}

/// The `Authorization: Bearer <key>` scheme — OpenAI and most of the vendors
/// that speak its protocol.
pub struct BearerAuth {
    key: String,
}

impl BearerAuth {
    /// Authenticate with this key.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }
}

#[async_trait]
impl ProviderAuth for BearerAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![(
            "authorization".to_string(),
            format!("Bearer {}", self.key),
        )])
    }
}

/// A credential carried in one named header — Anthropic's `x-api-key`, or
/// whatever a gateway asks for.
pub struct StaticHeaderAuth {
    name: String,
    value: String,
}

impl StaticHeaderAuth {
    /// Send `value` in the `name` header on every request.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[async_trait]
impl ProviderAuth for StaticHeaderAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![(self.name.clone(), self.value.clone())])
    }
}

/// Where one request goes and what it carries with it: the resolved result of
/// asking a [`Provider`] for its endpoint, immediately before a completion.
///
/// **Treat it as sensitive** — `headers` holds the credential the provider's
/// [`ProviderAuth`] just produced, so `Debug` redacts the values and this
/// must never reach an event or a log line.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct ProviderEndpoint {
    /// The service's base url, without a trailing slash. `None` for drivers
    /// that do not speak HTTP at all (the simulator).
    pub base_url: Option<String>,
    /// Credential and service headers, lowercase names.
    pub headers: Vec<(String, String)>,
}

impl fmt::Debug for ProviderEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEndpoint")
            .field("base_url", &self.base_url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// A service an agent can talk to: an id, the driver whose protocol it
/// speaks, and everything the protocol itself has no business knowing —
/// endpoint, credentials, service headers.
///
/// Build one and hand it to the agent; a
/// [`ModelSpec`](crate::driver::ModelSpec) names it by id.
///
/// ```
/// # use agentyk_core::{Provider, ProviderId};
/// # fn build(driver: impl agentyk_core::ChatDriver + 'static) {
/// let gateway = Provider::new("corp-gateway", driver)
///     .base_url("https://llm.corp.internal/anthropic")
///     .header("x-corp-cost-center", "research");
/// assert_eq!(gateway.id(), &ProviderId::new("corp-gateway"));
/// # }
/// ```
#[derive(Clone)]
pub struct Provider {
    id: ProviderId,
    driver: Arc<dyn ChatDriver>,
    base_url: Option<String>,
    auth: Option<Arc<dyn ProviderAuth>>,
    headers: Vec<(String, String)>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("auth", &self.auth.as_ref().map(|_| "<configured>"))
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Provider {
    /// A service under `id`, speaking `driver`'s protocol. Add the endpoint
    /// and credentials with the setters below.
    ///
    /// The bundled services have ready-made assemblies in the `agentyk`
    /// facade's `providers` module — reach for this one for a gateway, a
    /// proxy, or a local runtime.
    pub fn new(id: impl Into<ProviderId>, driver: impl ChatDriver + 'static) -> Self {
        Self::from_driver(id, Arc::new(driver))
    }

    /// A service speaking a driver you already hold as an `Arc` — the way to
    /// share one protocol implementation between several services.
    pub fn from_driver(id: impl Into<ProviderId>, driver: Arc<dyn ChatDriver>) -> Self {
        Self {
            id: id.into(),
            driver,
            base_url: None,
            auth: None,
            headers: Vec::new(),
        }
    }

    /// The scripted offline simulator, under the `"llmsim"` id
    /// [`ModelSpec::llmsim`](crate::driver::ModelSpec::llmsim) names. No
    /// endpoint, no credentials.
    pub fn llmsim(driver: impl ChatDriver + 'static) -> Self {
        Self::new(ProviderId::llmsim(), driver)
    }

    /// Where this service lives — the scheme and host the driver appends its
    /// endpoint path to. Required for any driver that speaks HTTP.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into().trim_end_matches('/').to_string());
        self
    }

    /// How this service authenticates. Asked per request — see
    /// [`ProviderAuth`].
    pub fn auth(mut self, auth: impl ProviderAuth + 'static) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// How this service authenticates, from an `Arc` you already hold —
    /// e.g. a token store shared with the rest of the host.
    pub fn auth_arc(mut self, auth: Arc<dyn ProviderAuth>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Send a fixed header on every request to this service — attribution,
    /// routing preferences, a cost-center tag. Not for credentials: those go
    /// through [`auth`](Self::auth), which is asked per request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .push((name.into().to_lowercase(), value.into()));
        self
    }

    /// Speak this service through a different protocol implementation,
    /// keeping its endpoint and credentials — a tuned driver, or a service
    /// that moved to a newer protocol. The escape hatch that keeps the
    /// ready-made providers usable when only the driver needs changing.
    pub fn with_driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.driver = Arc::new(driver);
        self
    }

    /// Speak this service through a driver you already hold as an `Arc` —
    /// see [`with_driver`](Self::with_driver).
    pub fn with_driver_arc(mut self, driver: Arc<dyn ChatDriver>) -> Self {
        self.driver = driver;
        self
    }

    /// The id a [`ModelSpec`](crate::driver::ModelSpec) routes to.
    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    /// The protocol implementation this service speaks.
    pub fn driver(&self) -> &Arc<dyn ChatDriver> {
        &self.driver
    }

    /// Resolve where the next request goes and what it carries — including
    /// asking [`ProviderAuth`] for fresh credentials.
    pub async fn endpoint(&self) -> Result<ProviderEndpoint> {
        let mut headers = self.headers.clone();
        if let Some(auth) = &self.auth {
            headers.extend(auth.headers().await?);
        }
        Ok(ProviderEndpoint {
            base_url: self.base_url.clone(),
            headers,
        })
    }

    /// One completion against this service: resolve the endpoint, then hand
    /// the request to the driver. The seam where credentials are attached, so
    /// a driver never sees a credential it did not need to know about.
    pub async fn complete(&self, mut request: ChatRequest) -> Result<ChatResponse> {
        request.endpoint = self.endpoint().await?;
        self.driver.complete(request).await
    }

    /// One streaming completion against this service — see
    /// [`complete`](Self::complete).
    pub async fn complete_streaming(
        &self,
        mut request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        request.endpoint = self.endpoint().await?;
        self.driver.complete_streaming(request, sink).await
    }
}

/// Routes a [`ModelSpec`](crate::driver::ModelSpec) to the [`Provider`] it
/// names.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<Provider>>,
}

impl ProviderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a provider, replacing any already registered under the same id.
    pub fn register(&mut self, provider: Provider) {
        self.providers
            .insert(provider.id.clone(), Arc::new(provider));
    }

    /// The provider registered under `id`, if any.
    pub fn get(&self, id: &ProviderId) -> Option<Arc<Provider>> {
        self.providers.get(id).cloned()
    }

    /// Whether a provider is registered under `id`.
    pub fn contains(&self, id: &ProviderId) -> bool {
        self.providers.contains_key(id)
    }

    /// Every registered id, sorted — what an "unknown provider" error lists
    /// so a typo in config is diagnosable without reading the wiring code.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().map(|id| id.to_string()).collect();
        ids.sort();
        ids
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopDriver;

    #[async_trait]
    impl ChatDriver for NoopDriver {
        async fn complete(&self, _request: ChatRequest) -> Result<ChatResponse> {
            unreachable!("core has no async runtime; the plumbing is tested in the facade")
        }
    }

    #[test]
    fn a_resolved_endpoint_does_not_debug_print_its_credentials() {
        let endpoint = ProviderEndpoint {
            base_url: Some("https://api.example.com".to_string()),
            headers: vec![(
                "authorization".to_string(),
                "Bearer super-secret".to_string(),
            )],
        };

        let debug = format!("{endpoint:?}");

        assert!(debug.contains("authorization"));
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn a_provider_does_not_debug_print_the_credential_it_holds() {
        let provider = Provider::new("gateway", NoopDriver)
            .base_url("https://llm.corp.internal/")
            .auth(BearerAuth::new("super-secret"));

        let debug = format!("{provider:?}");

        assert!(debug.contains("gateway"));
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn a_base_url_keeps_no_trailing_slash_so_drivers_can_append_a_path() {
        let provider = Provider::new("gateway", NoopDriver).base_url("https://llm.corp.internal/");

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://llm.corp.internal")
        );
    }

    #[test]
    fn a_registry_lists_its_ids_for_unknown_provider_errors() {
        let mut registry = ProviderRegistry::new();
        registry.register(Provider::new("openrouter", NoopDriver));
        registry.register(Provider::new("openai", NoopDriver));

        assert_eq!(registry.ids(), vec!["openai", "openrouter"]);
        assert!(registry.contains(&ProviderId::openai()));
        assert!(!registry.contains(&ProviderId::anthropic()));
    }
}
