//! Agent — a composed value, not a record.
//!
//! An [`Agent`] is name + system prompt + model + capabilities, built with
//! [`AgentBuilder`] entirely from values. There is no id to mint, nothing to
//! register, and no store to create it in. Sessions are spawned *from* the
//! agent; ids exist only inside them as correlation handles.

use std::sync::Arc;

use agentyk_core::capability::Capability;
use agentyk_core::driver::{ChatDriver, DriverRegistry, ModelSpec};
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventListener;
use agentyk_core::event_log::{EventLog, InMemoryEventLog};
use agentyk_core::executor::{TurnExecutor, TurnResult};
use agentyk_core::id::SessionId;
use agentyk_core::tool::Tool;
use async_trait::async_trait;

use crate::in_process::InProcessExecutor;
use crate::session::Session;

pub(crate) struct AgentInner {
    pub(crate) name: String,
    pub(crate) system_prompt: String,
    pub(crate) model: ModelSpec,
    pub(crate) capabilities: Vec<Arc<dyn Capability>>,
    pub(crate) drivers: DriverRegistry,
    pub(crate) listeners: Vec<Arc<dyn EventListener>>,
    pub(crate) max_iterations: usize,
    pub(crate) executor: Arc<dyn TurnExecutor>,
}

/// A runnable agent, composed by value. Cheap to clone (shared internals).
#[derive(Clone)]
pub struct Agent {
    pub(crate) inner: Arc<AgentInner>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.inner.name)
            .field("model", &self.inner.model)
            .field("capabilities", &self.inner.capabilities.len())
            .finish_non_exhaustive()
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn system_prompt(&self) -> &str {
        &self.inner.system_prompt
    }

    pub fn model(&self) -> &ModelSpec {
        &self.inner.model
    }

    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.inner.capabilities
    }

    pub fn drivers(&self) -> &DriverRegistry {
        &self.inner.drivers
    }

    /// The driver that speaks this agent's model protocol. Guaranteed by
    /// `build()` to exist.
    pub fn driver_for_model(&self) -> Option<Arc<dyn ChatDriver>> {
        self.inner.drivers.get(&self.inner.model.driver)
    }

    pub fn max_iterations(&self) -> usize {
        self.inner.max_iterations
    }

    /// Start a session with an in-memory event log.
    pub fn session(&self) -> Session {
        self.session_with_log(Arc::new(InMemoryEventLog::new()))
    }

    /// Start a session writing to the given event log.
    pub fn session_with_log(&self, log: Arc<dyn EventLog>) -> Session {
        Session::new(self.clone(), log)
    }

    /// Rebuild a session's message history by replaying its event log.
    pub async fn resume_session(
        &self,
        log: Arc<dyn EventLog>,
        session_id: SessionId,
    ) -> Result<Session> {
        Session::resume(self.clone(), log, session_id).await
    }

    /// One-shot convenience: run a single turn in a throwaway session.
    pub async fn run(&self, input: impl Into<String>) -> Result<TurnResult> {
        self.session().run(input).await
    }
}

/// Capability wrapping tools attached directly on the builder via
/// [`AgentBuilder::tool`].
struct AdHocTools {
    tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl Capability for AdHocTools {
    fn id(&self) -> &str {
        "tools"
    }

    fn description(&self) -> &str {
        "Tools attached directly to the agent."
    }

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(self.tools.clone())
    }
}

/// Fluent, by-value agent composition.
pub struct AgentBuilder {
    name: String,
    system_prompt: String,
    model: Option<ModelSpec>,
    capabilities: Vec<Arc<dyn Capability>>,
    tools: Vec<Arc<dyn Tool>>,
    drivers: DriverRegistry,
    listeners: Vec<Arc<dyn EventListener>>,
    max_iterations: usize,
    executor: Arc<dyn TurnExecutor>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            name: "agent".to_string(),
            system_prompt: String::new(),
            model: None,
            capabilities: Vec::new(),
            tools: Vec::new(),
            drivers: DriverRegistry::new(),
            listeners: Vec::new(),
            max_iterations: 16,
            executor: Arc::new(InProcessExecutor),
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// The model, by value — see [`ModelSpec`]. Required.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.model = Some(model);
        self
    }

    /// Attach a capability by object — no registry, no string reference.
    pub fn capability(mut self, capability: impl Capability + 'static) -> Self {
        self.capabilities.push(Arc::new(capability));
        self
    }

    pub fn capability_arc(mut self, capability: Arc<dyn Capability>) -> Self {
        self.capabilities.push(capability);
        self
    }

    /// Attach a single tool directly (wrapped in an implicit capability).
    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Register an extra or replacement LLM driver.
    pub fn driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.drivers.register(driver);
        self
    }

    pub fn listener(mut self, listener: impl EventListener + 'static) -> Self {
        self.listeners.push(Arc::new(listener));
        self
    }

    /// Attach a listener you already hold as an `Arc` — the way to keep a
    /// handle to inspect its state (e.g. a test collector) after the agent
    /// is built.
    pub fn listener_arc(mut self, listener: Arc<dyn EventListener>) -> Self {
        self.listeners.push(listener);
        self
    }

    /// Ceiling on reason/act iterations within one turn (default 16).
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Replace how turns execute (default: [`InProcessExecutor`]). Durable
    /// hosts plug in here — see [`agentyk_core::executor`].
    pub fn executor(mut self, executor: impl TurnExecutor + 'static) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    pub fn build(self) -> Result<Agent> {
        let model = self
            .model
            .ok_or_else(|| Error::InvalidAgent("a model is required — call .model(...)".into()))?;

        let mut capabilities = self.capabilities;
        if !self.tools.is_empty() {
            capabilities.push(Arc::new(AdHocTools { tools: self.tools }));
        }

        #[allow(unused_mut)]
        let mut drivers = self.drivers;
        #[cfg(feature = "http")]
        {
            use agentyk_core::driver::DriverId;
            if !drivers.contains(&DriverId::openai()) {
                drivers.register(crate::drivers::openai::OpenAiDriver::new());
            }
            if !drivers.contains(&DriverId::anthropic()) {
                drivers.register(crate::drivers::anthropic::AnthropicDriver::new());
            }
        }

        if !drivers.contains(&model.driver) {
            return Err(Error::UnknownDriver(model.driver.to_string()));
        }

        Ok(Agent {
            inner: Arc::new(AgentInner {
                name: self.name,
                system_prompt: self.system_prompt,
                model,
                capabilities,
                drivers,
                listeners: self.listeners,
                max_iterations: self.max_iterations,
                executor: self.executor,
            }),
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}
