//! Per-turn overrides — mirrors everruns' `Controls`.
//!
//! An agent's model is fixed at build time (`AgentBuilder::model`); most
//! turns want that. [`TurnControls`] is the escape hatch for the turns that
//! don't — a harder question that needs more reasoning effort, or a
//! specific input that should go to a different model — without rebuilding
//! the agent. Pass it to `Session::run_controlled` (or the combined
//! `Session::run_with_options`).

use serde::{Deserialize, Serialize};

use crate::driver::{ModelSpec, ReasoningConfig};

/// Per-turn overrides, applied on top of the agent's default model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnControls {
    /// Replace the agent's model entirely for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSpec>,
    /// Override reasoning effort, applied after `model` (or the agent
    /// default, if `model` is unset) is chosen — so you can bump effort for
    /// one turn without also overriding the model itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

impl TurnControls {
    /// No overrides — the agent's own model and settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run this turn on a different model, without rebuilding the agent.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.model = Some(model);
        self
    }

    /// Request a reasoning effort level for this turn, on whichever model
    /// ends up being used.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let mut reasoning = self.reasoning.unwrap_or_default();
        reasoning.effort = Some(effort.into());
        self.reasoning = Some(reasoning);
        self
    }

    /// Enable extended thinking with a token budget for this turn — see
    /// [`ModelSpec::thinking_budget`](crate::driver::ModelSpec::thinking_budget).
    pub fn thinking_budget(mut self, budget_tokens: u32) -> Self {
        let mut reasoning = self.reasoning.unwrap_or_default();
        reasoning.budget_tokens = Some(budget_tokens);
        self.reasoning = Some(reasoning);
        self
    }

    /// The model a turn should actually use: `self.model` (or
    /// `default_model`), with `self.reasoning` layered on top if set.
    pub fn resolve(&self, default_model: &ModelSpec) -> ModelSpec {
        let mut model = self.model.clone().unwrap_or_else(|| default_model.clone());
        if let Some(reasoning) = &self.reasoning {
            model.reasoning = Some(reasoning.clone());
        }
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::DriverId;

    #[test]
    fn no_controls_keeps_the_default_model() {
        let default_model = ModelSpec::new(DriverId::openai(), "gpt-5.5");
        let resolved = TurnControls::new().resolve(&default_model);
        assert_eq!(resolved, default_model);
    }

    #[test]
    fn model_override_replaces_the_default() {
        let default_model = ModelSpec::new(DriverId::openai(), "gpt-5.5");
        let override_model = ModelSpec::new(DriverId::anthropic(), "claude-sonnet-4-5");
        let resolved = TurnControls::new()
            .model(override_model.clone())
            .resolve(&default_model);
        assert_eq!(resolved, override_model);
    }

    #[test]
    fn reasoning_effort_layers_onto_whichever_model_is_chosen() {
        let default_model = ModelSpec::new(DriverId::openai(), "gpt-5.5");
        let resolved = TurnControls::new()
            .reasoning_effort("high")
            .resolve(&default_model);
        assert_eq!(resolved.model, "gpt-5.5");
        assert_eq!(resolved.reasoning.unwrap().effort, Some("high".to_string()));
    }
}
