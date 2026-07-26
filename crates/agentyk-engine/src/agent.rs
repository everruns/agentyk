//! Agent — a composed value, not a record.
//!
//! An [`Agent`] is name + system prompt + model + capabilities, built with
//! [`AgentBuilder`] entirely from values. There is no id to mint, nothing to
//! register, and no store to create it in. Sessions are spawned *from* the
//! agent; ids exist only inside them as correlation handles.
//!
//! Composition lives in one place: the builder produces a valid
//! [`AgentDefinition`] and [`AgentEnvironment`], which every
//! [`TurnHost`](crate::host::TurnHost) consumes directly.

use std::sync::Arc;

use agentyk_core::budget::BudgetChecker;
use agentyk_core::capability::Capability;
use agentyk_core::context::ContextAssembler;
use agentyk_core::driver::{ChatDriver, ModelSpec};
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventListener;
use agentyk_core::event_log::{EventLog, InMemoryEventLog};
use agentyk_core::id::SessionId;
use agentyk_core::message::Message;
use agentyk_core::middleware::TurnMiddleware;
use agentyk_core::profile::{ModelCatalog, ModelProfile};
use agentyk_core::tool::Tool;
use async_trait::async_trait;

use crate::config::AgentConfig;
use crate::host::TurnResult;
use crate::session::Session;

/// Agent behavior, separate from the resources that run it.
///
/// The model may contain credentials, so this value is sensitive
/// configuration even though durable events never include it.
#[derive(Clone)]
pub struct AgentDefinition {
    /// Human-readable diagnostic name.
    pub name: String,
    /// Base system instructions.
    pub instructions: String,
    /// Default model, required for every built definition.
    pub model: ModelSpec,
    /// Behavior attached by value.
    pub capabilities: Vec<Arc<dyn Capability>>,
    /// Maximum reason steps in one turn.
    pub max_iterations: usize,
    /// Ordered turn policy.
    pub middleware: Vec<Arc<dyn TurnMiddleware>>,
    /// Optional budget policy.
    pub budget_checker: Option<Arc<dyn BudgetChecker>>,
    /// Shapes durable history into model context.
    pub context_assembler: Arc<dyn ContextAssembler>,
    /// What this agent's model supports, when the host said — see
    /// [`AgentBuilder::model_catalog`]. Read it to size a context budget or
    /// to show a user what the model can do; the builder has already used it
    /// to validate the composition.
    pub model_profile: Option<ModelProfile>,
    /// Optional input-token ceiling exposed to context assembly.
    pub context_token_limit: Option<usize>,
}

impl std::fmt::Debug for AgentDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentDefinition")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("capabilities", &self.capabilities.len())
            .field("max_iterations", &self.max_iterations)
            .finish_non_exhaustive()
    }
}

/// Host resources used to execute an [`AgentDefinition`].
#[derive(Clone)]
pub struct AgentEnvironment {
    /// Provider implementations available to the host.
    pub drivers: agentyk_core::driver::DriverRegistry,
    /// Observers of durable and ephemeral events.
    pub listeners: Vec<Arc<dyn EventListener>>,
    /// Typed services made available to tools.
    pub extensions: agentyk_core::extensions::Extensions,
}

impl std::fmt::Debug for AgentEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentEnvironment")
            .field("listeners", &self.listeners.len())
            .finish_non_exhaustive()
    }
}

/// A runnable agent, composed by value. Cheap to clone (shared internals).
#[derive(Clone)]
pub struct Agent {
    definition: Arc<AgentDefinition>,
    environment: Arc<AgentEnvironment>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("definition", &self.definition)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Start composing an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// Portable definition of this agent.
    pub fn definition(&self) -> &AgentDefinition {
        &self.definition
    }

    /// Host resources used to run this agent.
    pub fn environment(&self) -> &AgentEnvironment {
        &self.environment
    }

    /// The agent's name, for logs and diagnostics.
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    /// The agent's default model. Per-turn overrides go through
    /// [`TurnControls`](agentyk_core::controls::TurnControls).
    pub fn model(&self) -> &ModelSpec {
        &self.definition.model
    }

    /// What the attached catalog says this agent's model supports, if
    /// anything — see [`AgentBuilder::model_catalog`].
    pub fn model_profile(&self) -> Option<&ModelProfile> {
        self.definition.model_profile.as_ref()
    }

    /// Everything attached to this agent, in attachment order.
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.definition.capabilities
    }

    /// The driver that speaks this agent's model protocol. Guaranteed by
    /// `build()` to exist.
    pub fn driver_for_model(&self) -> Option<Arc<dyn ChatDriver>> {
        self.environment.drivers.get(&self.definition.model.driver)
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
    pub async fn run(&self, input: impl Into<Message>) -> Result<TurnResult> {
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
}

impl AgentBuilder {
    /// A builder with defaults for everything but the model, which is
    /// required.
    pub fn new() -> Self {
        Self {
            config: AgentConfig::default(),
            tools: Vec::new(),
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

    /// Ceiling on reason/act iterations within one turn (default: 16).
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.config.max_iterations = max_iterations;
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

    /// Teach the builder what models support, so a composition the provider
    /// would reject fails here instead — see
    /// [`agentyk_core::profile::ModelCatalog`].
    ///
    /// Nothing is baked in: agentyk ships no model list, because a list in a
    /// library is stale by its next release and a wrong entry is worse than a
    /// missing one. A model the catalog does not know is passed through
    /// untouched, so attaching one can only reject combinations you described.
    ///
    /// A catalog states what a model takes, and `build()` refuses a model
    /// spec that contradicts it — `Error::InvalidAgent`, naming the supported
    /// values, before a turn ever runs:
    ///
    /// ```
    /// use agentyk_core::{DriverId, InMemoryModelCatalog, ModelCatalog, ModelProfile, ModelSpec};
    ///
    /// let catalog = InMemoryModelCatalog::new().with(
    ///     DriverId::openai(),
    ///     "gpt-5.5",
    ///     ModelProfile::new().reasoning_efforts(["low", "high"]),
    /// );
    ///
    /// let profile = catalog.profile(&DriverId::openai(), "gpt-5.5").unwrap();
    /// let error = profile
    ///     .validate(&ModelSpec::openai("gpt-5.5").reasoning_effort("medium"))
    ///     .unwrap_err();
    /// assert!(error.contains("low, high"));
    /// ```
    pub fn model_catalog(mut self, catalog: impl ModelCatalog + 'static) -> Self {
        self.config.model_catalog = Some(Arc::new(catalog));
        self
    }

    /// Attach a catalog you already hold as an `Arc` — the way to share one
    /// across agents.
    pub fn model_catalog_arc(mut self, catalog: Arc<dyn ModelCatalog>) -> Self {
        self.config.model_catalog = Some(catalog);
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

    /// Tell context assembly the maximum input-token budget for each model
    /// call. The assembler owns estimation and reduction; the engine does not
    /// silently trim messages.
    ///
    /// Left unset, an attached [`AgentBuilder::model_catalog`] can supply it:
    /// a profile stating **both** a context window and an output ceiling
    /// implies the input budget (window minus output). Only both, because
    /// with just the window there is no defensible number — reserving room
    /// for the response would mean inventing a figure the host never stated.
    pub fn context_token_limit(mut self, token_limit: usize) -> Self {
        self.config.context_token_limit = Some(token_limit);
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

        if !config.drivers.contains(&config.model().driver) {
            return Err(Error::UnknownDriver(config.model().driver.to_string()));
        }

        // Validated here rather than at the driver, so an unsupported
        // reasoning effort is a composition error with a message naming the
        // alternatives — not a provider 400 halfway through someone's turn.
        let model_profile = config
            .model_catalog
            .as_ref()
            .and_then(|catalog| catalog.profile(&config.model().driver, &config.model().model));
        if let Some(profile) = &model_profile
            && let Err(reason) = profile.validate(config.model())
        {
            return Err(Error::InvalidAgent(reason));
        }

        // A stated window and output ceiling are the input budget, so a host
        // that described its model does not also have to compute this.
        let context_token_limit = config.context_token_limit.or_else(|| {
            let profile = model_profile.as_ref()?;
            let window = usize::try_from(profile.context_window?).ok()?;
            let output = usize::try_from(profile.max_output_tokens?).ok()?;
            window.checked_sub(output)
        });

        let definition = AgentDefinition {
            name: config.name.clone(),
            instructions: config.system_prompt.clone(),
            model: config.model().clone(),
            capabilities: config.capabilities.clone(),
            max_iterations: config.max_iterations,
            middleware: config.middleware.clone(),
            budget_checker: config.budget_checker.clone(),
            context_assembler: config.context_assembler.clone(),
            model_profile,
            context_token_limit,
        };
        let environment = AgentEnvironment {
            drivers: config.drivers.clone(),
            listeners: config.listeners.clone(),
            extensions: config.extensions.clone(),
        };
        Ok(Agent {
            definition: Arc::new(definition),
            environment: Arc::new(environment),
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}
