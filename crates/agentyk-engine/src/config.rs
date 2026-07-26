//! Private mutable state used while building an agent.
//!
//! A built agent exposes only a valid definition and its runtime environment.

use std::sync::Arc;

use agentyk_core::budget::BudgetChecker;
use agentyk_core::capability::Capability;
use agentyk_core::context::{ContextAssembler, PassthroughContextAssembler};
use agentyk_core::driver::{DriverRegistry, ModelSpec};
use agentyk_core::event::EventListener;
use agentyk_core::extensions::Extensions;
use agentyk_core::middleware::TurnMiddleware;
use agentyk_core::profile::ModelCatalog;

/// The default ceiling on reason/act iterations within one turn.
pub(crate) const DEFAULT_MAX_ITERATIONS: usize = 16;

/// Mutable composition collected before the builder produces public values.
pub(crate) struct AgentConfig {
    /// Identifies the agent in logs and diagnostics. Not an id anything
    /// looks up — agentyk has no agent registry.
    pub(crate) name: String,
    /// The agent's own instructions. Capability contributions are appended
    /// to this when a turn assembles its prompt.
    pub(crate) system_prompt: String,
    /// `None` only while a builder is still filling this in — `AgentBuilder`
    /// refuses to build without one.
    pub(crate) model: Option<ModelSpec>,
    /// What the agent can do: each contributes prompt text and tools.
    pub(crate) capabilities: Vec<Arc<dyn Capability>>,
    /// The driver protocols available to this agent, routed by
    /// [`ModelSpec::driver`].
    pub(crate) drivers: DriverRegistry,
    /// Observers of the event stream. They cannot change a turn — see
    /// [`TurnMiddleware`] for that.
    pub(crate) listeners: Vec<Arc<dyn EventListener>>,
    /// Ceiling on reason/act iterations within one turn.
    pub(crate) max_iterations: usize,
    /// Interception inside the turn loop, in attachment order — see
    /// [`TurnMiddleware`]. One list, because one middleware often wants
    /// several of the points.
    pub(crate) middleware: Vec<Arc<dyn TurnMiddleware>>,
    /// Checked once per turn action, like cancellation. `None` means no
    /// budget policy.
    pub(crate) budget_checker: Option<Arc<dyn BudgetChecker>>,
    /// Transforms replayed history into what is actually sent to the model.
    pub(crate) context_assembler: Arc<dyn ContextAssembler>,
    /// Cloned into every [`agentyk_core::tool::ToolContext`] a turn constructs.
    pub(crate) extensions: Extensions,
    /// What the host knows about models, used to validate the composed one.
    /// `None` means no validation.
    pub(crate) model_catalog: Option<Arc<dyn ModelCatalog>>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "agent".to_string(),
            system_prompt: String::new(),
            model: None,
            capabilities: Vec::new(),
            drivers: DriverRegistry::new(),
            listeners: Vec::new(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            middleware: Vec::new(),
            budget_checker: None,
            context_assembler: Arc::new(PassthroughContextAssembler),
            extensions: Extensions::new(),
            model_catalog: None,
        }
    }
}

impl AgentConfig {
    /// The configured model. Panics only if called on a config a builder
    /// never finished — `AgentBuilder::build` rejects those, so this is
    /// infallible for any config reachable from an `Agent`.
    pub(crate) fn model(&self) -> &ModelSpec {
        self.model
            .as_ref()
            .expect("a built agent always has a model")
    }
}
