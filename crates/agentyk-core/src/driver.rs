//! LLM drivers — the multi-provider seam.
//!
//! Keeps the everruns driver vocabulary: a [`ChatDriver`] implements one
//! provider protocol, a [`DriverRegistry`] routes by [`DriverId`], and a
//! [`ModelSpec`] (everruns: `ResolvedModel`) is the by-value description of
//! "which model, through which driver, with which credentials" — no model or
//! provider ids, no registration ceremony.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;
use crate::tool::ToolDefinition;

/// Names a provider protocol implementation, e.g. `"openai"`, `"anthropic"`,
/// `"llmsim"`. An open string (not a closed enum) so embedders can register
/// their own drivers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriverId(String);

impl DriverId {
    /// Name a driver protocol. Use the constructors below for the bundled
    /// ones; this is for your own.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written on the wire and in a [`ModelSpec`].
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `"openai"` — the Chat Completions protocol, which many other
    /// providers also speak.
    pub fn openai() -> Self {
        Self::new("openai")
    }

    /// `"anthropic"` — the Messages protocol.
    pub fn anthropic() -> Self {
        Self::new("anthropic")
    }

    /// `"llmsim"` — the scripted offline simulator, for tests and examples.
    pub fn llmsim() -> Self {
        Self::new("llmsim")
    }
}

impl fmt::Display for DriverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DriverId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// Reasoning/thinking configuration for models that support it. Mirrors
/// everruns' `ReasoningConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct ReasoningConfig {
    /// Provider-defined effort level, e.g. `"low"` / `"medium"` / `"high"`.
    /// Honored by OpenAI-shaped drivers as `reasoning_effort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Token budget for extended thinking. Anthropic's thinking is enabled
    /// per-request with an integer budget (not an effort string), so it takes
    /// this rather than `effort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// A model, by value (everruns: `ResolvedModel`): wire model name, the driver
/// that speaks its protocol, and optional credentials/endpoint. Build it
/// inline and hand it to the agent — nothing to register.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelSpec {
    /// Which protocol implementation to route through.
    pub driver: DriverId,
    /// The provider's own model name, sent verbatim.
    pub model: String,
    /// Credential for this model. Some drivers require one
    /// (Anthropic); others accept none, for local runtimes and proxies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override the driver's default endpoint (OpenAI-compatible servers,
    /// proxies, local runtimes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Reasoning/thinking request, for models that support it. Honored per
    /// driver — see [`ReasoningConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

impl std::fmt::Debug for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSpec")
            .field("driver", &self.driver)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("reasoning", &self.reasoning)
            .finish()
    }
}

impl ModelSpec {
    /// A model on an explicitly named driver. Prefer [`ModelSpec::openai`]
    /// or [`ModelSpec::anthropic`] for the bundled ones.
    pub fn new(driver: impl Into<DriverId>, model: impl Into<String>) -> Self {
        Self {
            driver: driver.into(),
            model: model.into(),
            api_key: None,
            base_url: None,
            reasoning: None,
        }
    }

    /// A model served over the OpenAI Chat Completions protocol. Point it
    /// at any compatible endpoint with [`ModelSpec::base_url`].
    pub fn openai(model: impl Into<String>) -> Self {
        Self::new(DriverId::openai(), model)
    }

    /// A model served over the Anthropic Messages protocol.
    pub fn anthropic(model: impl Into<String>) -> Self {
        Self::new(DriverId::anthropic(), model)
    }

    /// The scripted offline simulator (the `SimDriver` in the `agentyk`
    /// framework crate).
    pub fn llmsim() -> Self {
        Self::new(DriverId::llmsim(), "llmsim")
    }

    /// Attach the credential this model authenticates with.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Request a reasoning/thinking effort level, where the driver supports
    /// it (currently honored by [`crate::driver::DriverId::openai`]-shaped
    /// drivers via `reasoning_effort`).
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let mut reasoning = self.reasoning.unwrap_or_default();
        reasoning.effort = Some(effort.into());
        self.reasoning = Some(reasoning);
        self
    }

    /// Enable extended thinking with a token budget, where the driver supports
    /// it (currently the Anthropic driver, which sends
    /// `thinking: { budget_tokens }`).
    pub fn thinking_budget(mut self, budget_tokens: u32) -> Self {
        let mut reasoning = self.reasoning.unwrap_or_default();
        reasoning.budget_tokens = Some(budget_tokens);
        self.reasoning = Some(reasoning);
        self
    }

    /// Send to a different endpoint than the driver's default — an
    /// OpenAI-compatible server, a proxy, a local runtime.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
}

/// One LLM completion request: everything a driver needs to produce the next
/// assistant message.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChatRequest {
    /// Which model, through which driver, with which credentials.
    pub model: ModelSpec,
    /// The assembled system prompt: the agent's, plus every capability's
    /// contribution.
    pub system_prompt: Option<String>,
    /// Conversation history, oldest first.
    pub messages: Vec<Message>,
    /// The tools offered to the model this turn. Deferred tools are
    /// executable but deliberately absent here.
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest {
    /// The minimum a completion needs: a model and a conversation.
    pub fn new(model: ModelSpec, messages: Vec<Message>) -> Self {
        Self {
            model,
            system_prompt: None,
            messages,
            tools: Vec::new(),
        }
    }

    /// Set the system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set the system prompt from an `Option`, for callers that assembled
    /// one that may be empty.
    pub fn maybe_system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Offer these tools to the model.
    pub fn tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

/// Token counts for one completion. Providers report more than this (cache
/// reads, reasoning tokens); the fields here are the two every provider has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub input_tokens: u64,
    /// Tokens produced by the model, reasoning tokens included where the
    /// provider counts them.
    pub output_tokens: u64,
}

impl Usage {
    /// Record one completion's token counts.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// Accumulate another completion's usage — how a turn totals its cost
    /// across iterations.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// What one completion produced.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChatResponse {
    /// The assistant message: final text and/or tool calls.
    pub message: Message,
    /// What it cost.
    pub usage: Usage,
}

impl ChatResponse {
    /// Pair a generated message with its token usage.
    pub fn new(message: Message, usage: Usage) -> Self {
        Self { message, usage }
    }
}

/// Receives incremental text as a [`ChatDriver`] streams a completion.
/// `accumulated` is the full text so far, including `delta` — the executor
/// forwards each call to the engine host's event recorder as an
/// ephemeral `output.message.delta` event.
#[async_trait]
pub trait DeltaSink: Send {
    /// Report newly generated text. `delta` is the increment; `accumulated`
    /// is everything so far, including it.
    ///
    /// Returning an error stops the stream — which is how cancellation takes
    /// effect mid-completion rather than after it.
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()>;
}

/// One provider protocol. Implementations translate [`ChatRequest`] to the
/// provider's wire format and back.
#[async_trait]
pub trait ChatDriver: Send + Sync {
    /// The protocol id this driver answers to; a [`ModelSpec`] naming it
    /// routes here.
    fn id(&self) -> DriverId;

    /// Produce the next assistant message.
    ///
    /// Errors should be [`crate::error::Error::Driver`] with a
    /// [`crate::error::LlmErrorKind`], so a host can tell a transient failure
    /// from a terminal one.
    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Stream a completion, reporting text as it arrives through `sink`, and
    /// return the same final [`ChatResponse`] [`complete`](Self::complete)
    /// would. The default forwards to `complete` and reports the whole
    /// response as a single delta — real incremental streaming is an
    /// opt-in per-driver upgrade (see the `http` feature's OpenAI/Anthropic
    /// drivers), not a requirement of the trait.
    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> Result<ChatResponse> {
        let response = self.complete(request).await?;
        let text = response.message.text();
        if !text.is_empty() {
            sink.delta(&text, &text).await?;
        }
        Ok(response)
    }
}

/// Routes a [`ModelSpec`] to the [`ChatDriver`] that speaks its protocol.
#[derive(Default, Clone)]
pub struct DriverRegistry {
    drivers: HashMap<DriverId, Arc<dyn ChatDriver>>,
}

impl DriverRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a driver, replacing any already registered under the same id.
    pub fn register(&mut self, driver: impl ChatDriver + 'static) {
        self.register_arc(Arc::new(driver));
    }

    /// Add a driver you already hold as an `Arc` — e.g. one shared between
    /// agents.
    pub fn register_arc(&mut self, driver: Arc<dyn ChatDriver>) {
        self.drivers.insert(driver.id(), driver);
    }

    /// The driver registered under `id`, if any.
    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn ChatDriver>> {
        self.drivers.get(id).cloned()
    }

    /// Whether a driver is registered under `id`.
    pub fn contains(&self, id: &DriverId) -> bool {
        self.drivers.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::ModelSpec;

    #[test]
    fn model_debug_redacts_credentials() {
        let model = ModelSpec::openai("test-model").api_key("super-secret");

        let debug = format!("{model:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("super-secret"));
    }
}
