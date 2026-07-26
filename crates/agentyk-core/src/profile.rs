//! What a model can actually do — and catching a request it can't, early.
//!
//! [`ModelSpec`] is deliberately just "which model,
//! through which driver, with which credentials". It will happily carry a
//! reasoning effort the model does not offer, or a thinking budget from a
//! model that has none; the first sign of trouble is then a provider error in
//! the middle of a turn, which is the most expensive place to learn it.
//!
//! A [`ModelCatalog`] is what a host consults instead. It is a **seam, not a
//! table**: agentyk ships no list of models on purpose. A baked-in catalog is
//! wrong within weeks of any provider's next release, and being wrong is worse
//! than being absent — it would reject a model that exists. Hosts already have
//! this data (a config file, an API call, a hard-coded map they own) and
//! implement the trait over it.
//!
//! What the library provides is the *check*: attach a catalog to an agent and
//! `build()` validates the model against it, so a bad combination fails at
//! composition time with a message naming what is supported.

use std::collections::HashMap;

use crate::driver::{DriverId, ModelSpec};

/// What one model supports. Every field is optional knowledge: `None` means
/// "not stated", never "not supported", so a partial catalog entry can still
/// be useful.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ModelProfile {
    /// Total context window in tokens, when known. A host renders budget and
    /// compaction decisions from this; core never enforces it.
    pub context_window: Option<u64>,
    /// Ceiling on tokens the model will generate in one response.
    pub max_output_tokens: Option<u64>,
    /// Reasoning-effort values this model accepts, e.g. `["low", "high"]`.
    /// Empty means the model takes no effort setting at all — which is what
    /// makes passing one an error rather than a no-op.
    pub reasoning_efforts: Vec<String>,
    /// Whether the model supports an extended-thinking token budget.
    pub thinking: bool,
    /// Whether the model accepts image content.
    pub vision: bool,
}

impl ModelProfile {
    /// A profile that states nothing. Build one up with the setters.
    pub fn new() -> Self {
        Self::default()
    }

    /// State the context window, in tokens.
    pub fn context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// State the per-response output ceiling, in tokens.
    pub fn max_output_tokens(mut self, tokens: u64) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// State which reasoning-effort values the model accepts.
    pub fn reasoning_efforts(
        mut self,
        efforts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.reasoning_efforts = efforts.into_iter().map(Into::into).collect();
        self
    }

    /// State that the model supports an extended-thinking budget.
    pub fn thinking(mut self, supported: bool) -> Self {
        self.thinking = supported;
        self
    }

    /// State that the model accepts images.
    pub fn vision(mut self, supported: bool) -> Self {
        self.vision = supported;
        self
    }

    /// Why this model cannot serve `model`, if it cannot.
    ///
    /// Checks only what the profile actually states — an empty profile
    /// accepts everything, so attaching a catalog can never reject a model it
    /// simply does not describe.
    pub fn validate(&self, model: &ModelSpec) -> Result<(), String> {
        let Some(reasoning) = &model.reasoning else {
            return Ok(());
        };

        if let Some(effort) = &reasoning.effort {
            if self.reasoning_efforts.is_empty() {
                return Err(format!(
                    "model `{}` takes no reasoning effort, but `{effort}` was requested",
                    model.model
                ));
            }
            if !self.reasoning_efforts.iter().any(|known| known == effort) {
                return Err(format!(
                    "model `{}` does not support reasoning effort `{effort}` (supported: {})",
                    model.model,
                    self.reasoning_efforts.join(", ")
                ));
            }
        }

        if reasoning.budget_tokens.is_some() && !self.thinking {
            return Err(format!(
                "model `{}` does not support extended thinking, but a thinking budget was set",
                model.model
            ));
        }

        Ok(())
    }
}

/// Where a host's knowledge about models lives.
///
/// Implement it over whatever you already have — a config file, a provider's
/// models endpoint, a map in your own source. Returning `None` means "I don't
/// know this model", which is never an error: an unknown model is sent to the
/// provider unchanged.
pub trait ModelCatalog: Send + Sync {
    /// What is known about this model on this driver.
    fn profile(&self, driver: &DriverId, model: &str) -> Option<ModelProfile>;
}

/// A catalog built from entries you supply — the straightforward
/// implementation, for hosts whose model knowledge is a literal list.
#[derive(Debug, Clone, Default)]
pub struct InMemoryModelCatalog {
    profiles: HashMap<(String, String), ModelProfile>,
}

impl InMemoryModelCatalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Describe one model. Replaces any previous entry for the same pair.
    ///
    /// ```
    /// use agentyk_core::{DriverId, InMemoryModelCatalog, ModelProfile, ModelSpec};
    ///
    /// let catalog = InMemoryModelCatalog::new().with(
    ///     DriverId::openai(),
    ///     "gpt-5.5",
    ///     ModelProfile::new()
    ///         .context_window(400_000)
    ///         .reasoning_efforts(["low", "medium", "high"]),
    /// );
    ///
    /// use agentyk_core::ModelCatalog;
    /// let profile = catalog.profile(&DriverId::openai(), "gpt-5.5").unwrap();
    /// assert!(profile.validate(&ModelSpec::openai("gpt-5.5").reasoning_effort("high")).is_ok());
    /// assert!(profile.validate(&ModelSpec::openai("gpt-5.5").reasoning_effort("extreme")).is_err());
    /// ```
    pub fn with(
        mut self,
        driver: DriverId,
        model: impl Into<String>,
        profile: ModelProfile,
    ) -> Self {
        self.profiles
            .insert((driver.as_str().to_string(), model.into()), profile);
        self
    }
}

impl ModelCatalog for InMemoryModelCatalog {
    fn profile(&self, driver: &DriverId, model: &str) -> Option<ModelProfile> {
        self.profiles
            .get(&(driver.as_str().to_string(), model.to_string()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt() -> ModelProfile {
        ModelProfile::new()
            .context_window(400_000)
            .reasoning_efforts(["low", "high"])
    }

    #[test]
    fn an_empty_profile_only_rejects_what_it_contradicts() {
        let empty = ModelProfile::new();
        // Nothing requested, nothing to disagree with.
        assert!(empty.validate(&ModelSpec::openai("mystery")).is_ok());
        // But "no efforts listed" is a statement, not a shrug: it means the
        // model takes none.
        assert!(
            empty
                .validate(&ModelSpec::openai("mystery").reasoning_effort("high"))
                .is_err()
        );
    }

    #[test]
    fn a_supported_effort_passes_and_an_unsupported_one_names_the_alternatives() {
        assert!(
            gpt()
                .validate(&ModelSpec::openai("gpt").reasoning_effort("high"))
                .is_ok()
        );
        let error = gpt()
            .validate(&ModelSpec::openai("gpt").reasoning_effort("ultra"))
            .expect_err("unsupported effort");
        assert!(error.contains("ultra"), "{error}");
        assert!(error.contains("low, high"), "{error}");
    }

    #[test]
    fn a_thinking_budget_needs_a_thinking_model() {
        let error = gpt()
            .validate(&ModelSpec::openai("gpt").thinking_budget(4_000))
            .expect_err("no thinking support");
        assert!(error.contains("extended thinking"), "{error}");

        let claude = ModelProfile::new().thinking(true);
        assert!(
            claude
                .validate(&ModelSpec::anthropic("claude").thinking_budget(4_000))
                .is_ok()
        );
    }

    #[test]
    fn a_catalog_is_keyed_by_driver_and_model() {
        let catalog = InMemoryModelCatalog::new().with(DriverId::openai(), "gpt", gpt());
        assert!(catalog.profile(&DriverId::openai(), "gpt").is_some());
        assert!(
            catalog.profile(&DriverId::anthropic(), "gpt").is_none(),
            "same name on another protocol is another model"
        );
        assert!(catalog.profile(&DriverId::openai(), "unknown").is_none());
    }
}
