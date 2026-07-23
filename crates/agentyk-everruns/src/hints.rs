//! Tool risk hints — an everruns-flavored taxonomy the satellite owns,
//! carried in the generic [`agentyk_core::tool::ToolDefinition::metadata`]
//! hatch under a `"hints"` key. Core knows nothing about this shape; this
//! module is the whole schema.

use std::sync::Arc;

use agentyk_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolPolicy};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// everruns' `readonly` / `destructive` / `open_world` per-tool risk flags.
/// Read by [`crate::EverrunsExecutor`] to decide what needs approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolHints {
    /// The tool only reads state; safe to run without gating.
    pub readonly: bool,
    /// The tool mutates or deletes state — the executor gates it behind the
    /// [`crate::Approver`].
    pub destructive: bool,
    /// The tool reaches outside the sandbox (network, other systems).
    pub open_world: bool,
}

impl ToolHints {
    pub fn readonly() -> Self {
        Self {
            readonly: true,
            ..Self::default()
        }
    }

    pub fn destructive() -> Self {
        Self {
            destructive: true,
            ..Self::default()
        }
    }

    /// Whether the executor should ask the [`crate::Approver`] before running.
    pub fn needs_approval(&self) -> bool {
        self.destructive || self.open_world
    }

    /// Read hints out of a tool definition's `metadata.hints`, if present.
    pub fn from_definition(definition: &ToolDefinition) -> Option<Self> {
        serde_json::from_value(definition.metadata.get("hints")?.clone()).ok()
    }

    /// Write these hints into a definition's `metadata.hints`.
    pub fn write_into(&self, definition: &mut ToolDefinition) {
        let hints = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        match &mut definition.metadata {
            serde_json::Value::Object(map) => {
                map.insert("hints".into(), hints);
            }
            slot => *slot = serde_json::json!({ "hints": hints }),
        }
    }
}

/// Wraps a tool to advertise [`ToolHints`] on its definition (in the
/// `metadata` hatch) without touching the tool's behavior — the ergonomic
/// way a satellite tags existing tools for risk gating.
pub struct HintedTool {
    inner: Arc<dyn Tool>,
    hints: ToolHints,
}

impl HintedTool {
    pub fn new(inner: impl Tool + 'static, hints: ToolHints) -> Self {
        Self {
            inner: Arc::new(inner),
            hints,
        }
    }

    pub fn wrap(inner: Arc<dyn Tool>, hints: ToolHints) -> Self {
        Self { inner, hints }
    }
}

#[async_trait]
impl Tool for HintedTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        self.hints.write_into(&mut definition);
        definition
    }

    async fn execute(&self, arguments: serde_json::Value, context: &ToolContext) -> ToolOutput {
        self.inner.execute(arguments, context).await
    }

    fn policy(&self) -> ToolPolicy {
        self.inner.policy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hints_round_trip_through_definition_metadata() {
        let mut def = ToolDefinition::new("rm", "Delete a file.", json!({"type": "object"}));
        assert_eq!(ToolHints::from_definition(&def), None);

        ToolHints::destructive().write_into(&mut def);
        let read = ToolHints::from_definition(&def).unwrap();
        assert!(read.destructive);
        assert!(read.needs_approval());
        // Written under the shared `metadata` hatch, not a typed core field.
        assert!(def.metadata["hints"]["destructive"].as_bool().unwrap());
    }
}
