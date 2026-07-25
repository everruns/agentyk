//! Agent — a composed value, not a record.
//!
//! An [`Agent`] is name + system prompt + model + capabilities, built with
//! [`AgentBuilder`] entirely from values. There is no id to mint, nothing to
//! register, and no store to create it in. Sessions are spawned *from* the
//! agent; ids exist only inside them as correlation handles.
//!
//! Composition lives in one place: the builder fills an
//! [`AgentConfig`] and hands it over. A new knob is a field on `AgentConfig`
//! plus a setter here — it reaches every executor through
//! [`TurnHost`](agentyk_core::executor::TurnHost) without any further wiring.

use std::sync::Arc;

use agentyk_core::budget::BudgetChecker;
use agentyk_core::capability::Capability;
use agentyk_core::config::AgentConfig;
use agentyk_core::context::ContextAssembler;
use agentyk_core::driver::{ChatDriver, DriverRegistry, ModelSpec};
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventListener;
use agentyk_core::event_log::{EventLog, InMemoryEventLog};
use agentyk_core::executor::{TurnExecutor, TurnResult};
use agentyk_core::id::SessionId;
use agentyk_core::middleware::TurnMiddleware;
use agentyk_core::tool::Tool;
use async_trait::async_trait;

use crate::in_process::InProcessExecutor;
use crate::session::Session;

/// A runnable agent, composed by value. Cheap to clone (shared internals).
#[derive(Clone)]
pub struct Agent {
    pub(crate) config: Arc<AgentConfig>,
    // Deliberately *not* part of `AgentConfig`: the executor is what consumes
    // a composition, not part of one.
    pub(crate) executor: Arc<dyn TurnExecutor>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Start composing an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// Everything this agent is composed of. Prefer this over per-field
    /// accessors — it is the one thing that grows.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// The agent's name, for logs and diagnostics.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// The agent's default model. Per-turn overrides go through
    /// [`TurnControls`](agentyk_core::controls::TurnControls).
    pub fn model(&self) -> &ModelSpec {
        self.config.model()
    }

    /// Everything attached to this agent, in attachment order.
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.config.capabilities
    }

    /// The driver that speaks this agent's model protocol. Guaranteed by
    /// `build()` to exist.
    pub fn driver_for_model(&self) -> Option<Arc<dyn ChatDriver>> {
        self.config.driver_for_model()
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
    config: AgentConfig,
    /// Staged until `build()` wraps them in an implicit capability.
    tools: Vec<Arc<dyn Tool>>,
    executor: Arc<dyn TurnExecutor>,
}

impl AgentBuilder {
    /// A builder with defaults for everything but the model, which is
    /// required.
    pub fn new() -> Self {
        Self {
            config: AgentConfig::default(),
            tools: Vec::new(),
            executor: Arc::new(InProcessExecutor),
        }
    }

    /// Name the agent, for logs and diagnostics. Defaults to `"agent"`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// The agent's own instructions. Capability contributions are appended
    /// to this when a turn assembles its prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.config.system_prompt = prompt.into();
        self
    }

    /// The model, by value — see [`ModelSpec`]. Required.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.config.model = Some(model);
        self
    }

    /// Attach a capability by object — no registry, no string reference.
    pub fn capability(mut self, capability: impl Capability + 'static) -> Self {
        self.config.capabilities.push(Arc::new(capability));
        self
    }

    /// Attach a capability you already hold as an `Arc` — the way to share
    /// one between agents, or keep a handle on its state.
    pub fn capability_arc(mut self, capability: Arc<dyn Capability>) -> Self {
        self.config.capabilities.push(capability);
        self
    }

    /// Attach a single tool directly (wrapped in an implicit capability).
    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Register an extra or replacement LLM driver.
    pub fn driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.config.drivers.register(driver);
        self
    }

    /// Observe every event this agent's sessions emit. Listeners cannot
    /// change a turn — see [`AgentBuilder::middleware`] for that.
    pub fn listener(mut self, listener: impl EventListener + 'static) -> Self {
        self.config.listeners.push(Arc::new(listener));
        self
    }

    /// Attach a listener you already hold as an `Arc` — the way to keep a
    /// handle to inspect its state (e.g. a test collector) after the agent
    /// is built.
    pub fn listener_arc(mut self, listener: Arc<dyn EventListener>) -> Self {
        self.config.listeners.push(listener);
        self
    }

    /// Ceiling on reason/act iterations within one turn (default
    /// [`DEFAULT_MAX_ITERATIONS`](agentyk_core::config::DEFAULT_MAX_ITERATIONS)).
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.config.max_iterations = max_iterations;
        self
    }

    /// Replace how turns execute (default: [`InProcessExecutor`]). Durable
    /// hosts plug in here — see [`agentyk_core::executor`].
    pub fn executor(mut self, executor: impl TurnExecutor + 'static) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    /// Intercept points inside the turn loop, in attachment order — deny a
    /// tool call, rewrite it before it runs, or transform its output. What
    /// approval gating, guardrails, and redaction are built on; see
    /// [`agentyk_core::middleware::TurnMiddleware`].
    pub fn middleware(mut self, middleware: impl TurnMiddleware + 'static) -> Self {
        self.config.middleware.push(Arc::new(middleware));
        self
    }

    /// Attach middleware you already hold as an `Arc` — the way to keep a
    /// handle on it (e.g. an approver whose state a test inspects).
    pub fn middleware_arc(mut self, middleware: Arc<dyn TurnMiddleware>) -> Self {
        self.config.middleware.push(middleware);
        self
    }

    /// Stop a turn deliberately rather than letting it run to
    /// `MaxIterations` — checked once per action, like cancellation. See
    /// [`agentyk_core::budget::BudgetChecker`]. No default policy: unset
    /// means a turn never seals for budget reasons.
    pub fn budget_checker(mut self, checker: impl BudgetChecker + 'static) -> Self {
        self.config.budget_checker = Some(Arc::new(checker));
        self
    }

    /// Replace how replayed history is turned into what's sent to the model
    /// each turn (default: send it unchanged) — trimming, compaction, and
    /// memory injection are implementations of this seam, not changes to
    /// the turn machine. See [`agentyk_core::context::ContextAssembler`].
    pub fn context_assembler(mut self, assembler: impl ContextAssembler + 'static) -> Self {
        self.config.context_assembler = Arc::new(assembler);
        self
    }

    /// Make a service available to every tool call via
    /// [`agentyk_core::tool::ToolContext::extensions`] — a credential
    /// store, a workspace handle, anything a tool needs but core shouldn't
    /// know the shape of. Replaces any previous value of the same type.
    pub fn extension<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.config.extensions.insert(value);
        self
    }

    /// Finish composing.
    ///
    /// Fails if no model was set, or if no driver is registered for the one
    /// that was — both are wiring mistakes worth catching before a turn runs
    /// rather than during one.
    pub fn build(self) -> Result<Agent> {
        let mut config = self.config;

        if config.model.is_none() {
            return Err(Error::InvalidAgent(
                "a model is required — call .model(...)".into(),
            ));
        }

        if !self.tools.is_empty() {
            config
                .capabilities
                .push(Arc::new(AdHocTools { tools: self.tools }));
        }

        register_bundled_drivers(&mut config.drivers);

        if !config.drivers.contains(&config.model().driver) {
            return Err(Error::UnknownDriver(config.model().driver.to_string()));
        }

        Ok(Agent {
            config: Arc::new(config),
            executor: self.executor,
        })
    }
}

/// The HTTP drivers are registered unless the agent already supplied its own
/// under the same id, so `ModelSpec::openai(..)` works with no wiring.
#[cfg_attr(not(feature = "http"), allow(unused_variables))]
fn register_bundled_drivers(drivers: &mut DriverRegistry) {
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
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}
