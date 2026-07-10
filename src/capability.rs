//! Capabilities — composable extensions that give an agent behavior.
//!
//! A capability contributes system-prompt text and tools. It keeps the
//! everruns `Capability` trait shape (`id`, `system_prompt_contribution`,
//! `tools`), with one deliberate difference: capabilities are attached to an
//! agent **by object** (`AgentBuilder::capability(FileSystem::new())`), not
//! registered in a registry and then referenced by string id. The string
//! `id()` remains for diagnostics and for hosts that layer config-driven
//! wiring on top.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::id::SessionId;
use crate::tool::Tool;

/// Context available while assembling the system prompt.
#[derive(Debug, Clone)]
pub struct SystemPromptContext {
    pub session_id: SessionId,
}

#[async_trait]
pub trait Capability: Send + Sync {
    /// Stable string id, e.g. `"file_system"` or `"mcp:github"`.
    fn id(&self) -> &str;

    fn name(&self) -> &str {
        self.id()
    }

    fn description(&self) -> &str {
        ""
    }

    /// Text appended to the agent's system prompt, if any.
    async fn system_prompt_contribution(&self, _context: &SystemPromptContext) -> Option<String> {
        None
    }

    /// Tools this capability exposes to the model. Async so capabilities that
    /// discover tools remotely (MCP) fit the same trait.
    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(Vec::new())
    }
}

/// Internal capability wrapping tools attached directly on the builder via
/// [`crate::agent::AgentBuilder::tool`].
pub(crate) struct AdHocTools {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
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
