//! OpenRouter driver over its Open Responses-compatible API.

use std::sync::Arc;

use async_trait::async_trait;

use agentyk_core::driver::{
    AuthHeaderProvider, ChatDriver, ChatRequest, ChatResponse, DeltaSink, DriverId,
};
use agentyk_core::error::Result;

use super::openresponses::OpenResponsesDriver;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Speaks OpenRouter's Open Responses-compatible protocol.
///
/// Unlike an [`crate::drivers::openai::OpenAiDriver`] with a changed base URL,
/// this has its own driver id and uses `/responses`, so provider-specific model
/// catalogs and configuration remain correctly attributed to OpenRouter.
pub struct OpenRouterDriver {
    inner: OpenResponsesDriver,
}

impl OpenRouterDriver {
    /// A driver registered as `"openrouter"` on a default HTTP client.
    pub fn new() -> Self {
        Self {
            inner: OpenResponsesDriver::for_provider(
                DriverId::openrouter(),
                "openrouter",
                DEFAULT_BASE_URL,
                true,
            ),
        }
    }

    /// Supply your own client for timeouts, proxies, and connection pooling.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.inner = self.inner.with_client(client);
        self
    }

    /// Resolve authentication immediately before every request.
    ///
    /// This supports both refreshable bearer tokens and OpenRouter's
    /// connect flow, whose resulting key can be returned as a bearer header.
    pub fn with_auth_provider(mut self, provider: impl AuthHeaderProvider + 'static) -> Self {
        self.inner = self.inner.with_auth_provider(provider);
        self
    }

    /// Attach a shared request-time authentication provider.
    pub fn with_auth_provider_arc(mut self, provider: Arc<dyn AuthHeaderProvider>) -> Self {
        self.inner = self.inner.with_auth_provider_arc(provider);
        self
    }
}

impl Default for OpenRouterDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChatDriver for OpenRouterDriver {
    fn id(&self) -> DriverId {
        DriverId::openrouter()
    }

    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        self.inner.complete_streaming(request, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_first_class_driver_id() {
        assert_eq!(OpenRouterDriver::new().id(), DriverId::openrouter());
    }
}
