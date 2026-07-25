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
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::id::SessionId;
use crate::tool::{Tool, ToolOutput};

/// Context available while assembling the system prompt.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SystemPromptContext {
    pub session_id: SessionId,
}

impl SystemPromptContext {
    pub fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }
}

/// One slash-command a capability exposes — everruns' `CommandDescriptor`,
/// pared down to what a host needs to list and invoke it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CommandDescriptor {
    /// Invoked as `/{name}` (no leading slash here).
    pub name: String,
    pub description: String,
}

impl CommandDescriptor {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Context available while executing a command. Commands are host-invoked
/// directly (e.g. a user typing `/goal set X`) and bypass the turn loop
/// entirely, so there's no `turn_id` — only a session.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommandContext {
    pub session_id: SessionId,
}

impl CommandContext {
    pub fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }
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

    /// Generic, serializable extension hatch for host-side capability
    /// metadata a satellite crate owns — enabled/degraded `status`,
    /// `category`, `icon`, and similar richness everruns exposes. Kept as an
    /// opaque bag (default `Null`) rather than typed core methods, so the
    /// contract doesn't grow per host concern; a host reads and interprets
    /// the shape it defined.
    fn metadata(&self) -> serde_json::Value {
        serde_json::Value::Null
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

    /// Slash commands this capability exposes for a host to list and route
    /// — see `Session::commands`/`Session::execute_command` in the
    /// framework crate. Default: none.
    fn commands(&self) -> Vec<CommandDescriptor> {
        Vec::new()
    }

    /// Run a command by name. Return `None` if this capability doesn't own
    /// `name` (the host tries the next capability); `args` is the raw text
    /// after the command name.
    async fn execute_command(
        &self,
        _name: &str,
        _args: &str,
        _context: &CommandContext,
    ) -> Option<ToolOutput> {
        None
    }
}
