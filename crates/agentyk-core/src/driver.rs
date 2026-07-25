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
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn openai() -> Self {
        Self::new("openai")
    }

    pub fn anthropic() -> Self {
        Self::new("anthropic")
    }

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelSpec {
    pub driver: DriverId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override the driver's default endpoint (OpenAI-compatible servers,
    /// proxies, local runtimes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

impl ModelSpec {
    pub fn new(driver: impl Into<DriverId>, model: impl Into<String>) -> Self {
        Self {
            driver: driver.into(),
            model: model.into(),
            api_key: None,
            base_url: None,
            reasoning: None,
        }
    }

    pub fn openai(model: impl Into<String>) -> Self {
        Self::new(DriverId::openai(), model)
    }

    pub fn anthropic(model: impl Into<String>) -> Self {
        Self::new(DriverId::anthropic(), model)
    }

    /// The scripted offline simulator (the `SimDriver` in the `agentyk`
    /// framework crate).
    pub fn llmsim() -> Self {
        Self::new(DriverId::llmsim(), "llmsim")
    }

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
    pub model: ModelSpec,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest {
    pub fn new(model: ModelSpec, messages: Vec<Message>) -> Self {
        Self {
            model,
            system_prompt: None,
            messages,
            tools: Vec::new(),
        }
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn maybe_system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt;
        self
    }

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
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChatResponse {
    /// The assistant message: final text and/or tool calls.
    pub message: Message,
    pub usage: Usage,
}

impl ChatResponse {
    pub fn new(message: Message, usage: Usage) -> Self {
        Self { message, usage }
    }
}

/// Receives incremental text as a [`ChatDriver`] streams a completion.
/// `accumulated` is the full text so far, including `delta` — the executor
/// forwards each call straight to [`crate::executor::TurnHost::record`] as an
/// ephemeral `output.message.delta` event.
#[async_trait]
pub trait DeltaSink: Send {
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()>;
}

/// One provider protocol. Implementations translate [`ChatRequest`] to the
/// provider's wire format and back.
#[async_trait]
pub trait ChatDriver: Send + Sync {
    fn id(&self) -> DriverId;

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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, driver: impl ChatDriver + 'static) {
        self.register_arc(Arc::new(driver));
    }

    pub fn register_arc(&mut self, driver: Arc<dyn ChatDriver>) {
        self.drivers.insert(driver.id(), driver);
    }

    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn ChatDriver>> {
        self.drivers.get(id).cloned()
    }

    pub fn contains(&self, id: &DriverId) -> bool {
        self.drivers.contains_key(id)
    }
}
