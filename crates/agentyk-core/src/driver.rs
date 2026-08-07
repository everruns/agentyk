//! LLM drivers — the protocol seam.
//!
//! A [`ChatDriver`] implements one provider protocol and nothing else: build
//! a request body, read a response, fold a stream. Where that request is
//! sent and what credential it carries belong to the
//! [`Provider`](crate::provider::Provider) that speaks the protocol — one
//! wire format is spoken by many services, so a driver that knew a hostname
//! or an api key could only ever serve one of them.
//!
//! A [`ModelSpec`] (everruns: `ResolvedModel`) is the by-value description of
//! "which model, on which service" — no credentials, no registration
//! ceremony.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;
use crate::provider::{ProviderEndpoint, ProviderId};
use crate::tool::ToolDefinition;

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

/// A model, by value (everruns: `ResolvedModel`): the wire model name and the
/// service it runs on. Build it inline and hand it to the agent — nothing to
/// register.
///
/// It carries **no credentials**. That is deliberate and load-bearing: a
/// `ModelSpec` is ordinary configuration, safe to deserialize from a file, to
/// log, and to put in an event. The key lives on the
/// [`Provider`](crate::provider::Provider) this names, which is a runtime
/// object that was never serializable in the first place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelSpec {
    /// The service to run this model on — resolved against the agent's
    /// [`ProviderRegistry`](crate::provider::ProviderRegistry).
    pub provider: ProviderId,
    /// The provider's own model name, sent verbatim.
    pub model: String,
    /// Reasoning/thinking request, for models that support it. Honored per
    /// driver — see [`ReasoningConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// Generic, serializable hatch for model-flavored request configuration
    /// core should not grow a field for — everruns' `ProviderMetadata`, in
    /// the shape [`crate::tool::ToolDefinition::metadata`] established. A
    /// driver that understands a knob reads it here rather than every adopter
    /// waiting on a core release for a field only one service uses.
    ///
    /// Per-*model* configuration only, and never a credential: service-wide
    /// settings belong on the provider, and credentials on its
    /// [`ProviderAuth`](crate::provider::ProviderAuth).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl ModelSpec {
    /// A model on an explicitly named service. Prefer [`ModelSpec::openai`]
    /// or [`ModelSpec::anthropic`] for the bundled ones.
    ///
    /// The id must match a [`Provider`](crate::provider::Provider)
    /// registered on the agent, or `build()` fails naming the ids that are.
    pub fn on(provider: impl Into<ProviderId>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning: None,
            metadata: serde_json::Value::Null,
        }
    }

    /// A model on OpenAI itself — the `"openai"` provider.
    pub fn openai(model: impl Into<String>) -> Self {
        Self::on(ProviderId::openai(), model)
    }

    /// A model on Anthropic itself — the `"anthropic"` provider.
    pub fn anthropic(model: impl Into<String>) -> Self {
        Self::on(ProviderId::anthropic(), model)
    }

    /// The scripted offline simulator (the `SimDriver` in the `agentyk`
    /// framework crate, behind [`Provider::llmsim`](crate::provider::Provider::llmsim)).
    pub fn llmsim() -> Self {
        Self::on(ProviderId::llmsim(), "llmsim")
    }

    /// Request a reasoning/thinking effort level, where the driver supports
    /// it (currently honored by OpenAI-shaped drivers via
    /// `reasoning_effort`).
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

    /// Attach model-flavored configuration a driver knows how to read — see
    /// [`ModelSpec::metadata`].
    ///
    /// ```
    /// # use agentyk_core::ModelSpec;
    /// let model = ModelSpec::openai("gpt-5.5")
    ///     .metadata(serde_json::json!({ "service_tier": "flex" }));
    /// assert_eq!(model.metadata["service_tier"], "flex");
    /// ```
    pub fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// One LLM completion request: everything a driver needs to produce the next
/// assistant message.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChatRequest {
    /// Which model, on which service.
    pub model: ModelSpec,
    /// Where this request goes and what it carries — filled in by
    /// [`Provider::complete`](crate::provider::Provider::complete)
    /// immediately before the driver sees the request, so credentials are
    /// resolved per call rather than frozen at composition time. Empty for
    /// drivers that speak no HTTP.
    pub endpoint: ProviderEndpoint,
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
            endpoint: ProviderEndpoint::default(),
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

/// Token counts for one completion.
///
/// `input_tokens`/`output_tokens` are the totals every provider reports. The
/// remaining fields are *breakdowns* of those totals — subsets, never addends:
/// cached and cache-writing tokens are part of `input_tokens`, reasoning
/// tokens part of `output_tokens`. They are zero when the provider does not
/// report them.
///
/// The breakdown is here rather than left to each host because it is the only
/// evidence a caller has that prompt caching worked: the driver places
/// `cache_control` breakpoints, and without `cache_read_input_tokens` a host
/// cannot tell a cache hit from a cache miss — the tokens are billed at very
/// different rates but look identical in the total. Evals grade it; the same
/// numbers drive cost accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Tokens consumed by the prompt, cached ones included.
    pub input_tokens: u64,
    /// Tokens produced by the model, reasoning tokens included where the
    /// provider counts them.
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache — a subset of
    /// [`input_tokens`](Self::input_tokens). Zero when unreported.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Prompt tokens written to the provider's cache this request — a subset
    /// of [`input_tokens`](Self::input_tokens), billed at a premium. Zero when
    /// unreported.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Reasoning/thinking tokens — a subset of
    /// [`output_tokens`](Self::output_tokens). Zero when unreported.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Record one completion's token totals, with no breakdown.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            ..Self::default()
        }
    }

    /// Attach the prompt-cache breakdown: tokens served from the cache and
    /// tokens written to it. Both are already counted in `input_tokens`.
    pub fn with_cache(mut self, read: u64, creation: u64) -> Self {
        self.cache_read_input_tokens = read;
        self.cache_creation_input_tokens = creation;
        self
    }

    /// Attach the reasoning-token breakdown, already counted in
    /// `output_tokens`.
    pub fn with_reasoning(mut self, reasoning_tokens: u64) -> Self {
        self.reasoning_tokens = reasoning_tokens;
        self
    }

    /// Accumulate another completion's usage — how a turn totals its cost
    /// across iterations.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
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
/// wire format and back — and nothing else: a driver is named by the
/// [`Provider`](crate::provider::Provider) that holds it, so it needs no id
/// of its own, and it reads its endpoint and credentials from
/// [`ChatRequest::endpoint`] rather than owning any.
#[async_trait]
pub trait ChatDriver: Send + Sync {
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

#[cfg(test)]
mod tests {
    use super::ModelSpec;

    #[test]
    fn a_model_spec_is_ordinary_config_and_round_trips_as_such() {
        // No credential field to redact: the whole reason a spec can be
        // serialized, logged, and checked into config.
        let model = ModelSpec::openai("gpt-5.5").reasoning_effort("high");

        let json = serde_json::to_string(&model).unwrap();

        assert_eq!(
            json,
            r#"{"provider":"openai","model":"gpt-5.5","reasoning":{"effort":"high"}}"#
        );
        assert_eq!(serde_json::from_str::<ModelSpec>(&json).unwrap(), model);
    }
}
