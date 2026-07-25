//! Agent composition, as one value.
//!
//! [`AgentConfig`] is everything an agent *is* — prompt, model, capabilities,
//! drivers, hooks, and the seams a host plugs into. `agentyk`'s `AgentBuilder`
//! fills one in and `Agent` holds it behind an `Arc`; a [`TurnExecutor`](crate::executor::TurnExecutor) reads
//! it through [`TurnHost`](crate::executor::TurnHost).
//!
//! It lives in core, not in the framework, because it is what the executor
//! seam is defined against: a durable host implementing `TurnExecutor` needs
//! the agent's composition without depending on the builder that produced it.
//!
//! Composition knobs are added **here and nowhere else**. Before this type,
//! one new knob (a budget checker, a context assembler) had to be threaded
//! through a builder field, a builder setter, a mirror struct, a `build()`
//! copy, an accessor, a `TurnHost` field, and the session's per-turn wiring —
//! seven edits across four files, every one of them mechanical, and the
//! `TurnHost` field made it a breaking change for anyone else's executor. Now
//! it is a field with a default plus a builder setter.

use std::sync::Arc;

use crate::budget::BudgetChecker;
use crate::capability::Capability;
use crate::context::{ContextAssembler, PassthroughContextAssembler};
use crate::driver::{ChatDriver, DriverRegistry, ModelSpec};
use crate::event::EventListener;
use crate::extensions::Extensions;
use crate::middleware::TurnMiddleware;

/// The default ceiling on reason/act iterations within one turn.
pub const DEFAULT_MAX_ITERATIONS: usize = 16;

/// An agent's composition. Cheap to share (`Arc`), never mutated after the
/// builder hands it over.
#[non_exhaustive]
pub struct AgentConfig {
    /// Identifies the agent in logs and diagnostics. Not an id anything
    /// looks up — agentyk has no agent registry.
    pub name: String,
    /// The agent's own instructions. Capability contributions are appended
    /// to this when a turn assembles its prompt.
    pub system_prompt: String,
    /// `None` only while a builder is still filling this in — `AgentBuilder`
    /// refuses to build without one, so an `AgentConfig` reachable from an
    /// `Agent` always carries a model. Use [`AgentConfig::model`].
    pub model: Option<ModelSpec>,
    /// What the agent can do: each contributes prompt text and tools.
    pub capabilities: Vec<Arc<dyn Capability>>,
    /// The driver protocols available to this agent, routed by
    /// [`ModelSpec::driver`].
    pub drivers: DriverRegistry,
    /// Observers of the event stream. They cannot change a turn — see
    /// [`TurnMiddleware`](crate::middleware::TurnMiddleware) for that.
    pub listeners: Vec<Arc<dyn EventListener>>,
    /// Ceiling on reason/act iterations within one turn.
    pub max_iterations: usize,
    /// Interception inside the turn loop, in attachment order — see
    /// [`TurnMiddleware`]. One list, because one middleware often wants
    /// several of the points.
    pub middleware: Vec<Arc<dyn TurnMiddleware>>,
    /// Checked once per turn action, like cancellation. `None` means no
    /// budget policy.
    pub budget_checker: Option<Arc<dyn BudgetChecker>>,
    /// Transforms replayed history into what is actually sent to the model.
    pub context_assembler: Arc<dyn ContextAssembler>,
    /// Cloned into every [`crate::tool::ToolContext`] a turn constructs.
    pub extensions: Extensions,
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
        }
    }
}

impl AgentConfig {
    /// The configured model. Panics only if called on a config a builder
    /// never finished — `AgentBuilder::build` rejects those, so this is
    /// infallible for any config reachable from an `Agent`.
    pub fn model(&self) -> &ModelSpec {
        self.model
            .as_ref()
            .expect("a built agent always has a model")
    }

    /// The driver that speaks this agent's model protocol. Guaranteed to
    /// exist by `AgentBuilder::build`.
    pub fn driver_for_model(&self) -> Option<Arc<dyn ChatDriver>> {
        self.drivers.get(&self.model().driver)
    }
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("capabilities", &self.capabilities.len())
            .field("max_iterations", &self.max_iterations)
            .finish_non_exhaustive()
    }
}
