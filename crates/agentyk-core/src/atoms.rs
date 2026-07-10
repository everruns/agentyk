//! Atoms — the stateless, effectful operations a host runs between
//! [`crate::turn::TurnState`] transitions.
//!
//! Adopted from everruns-core's atoms (`input`/`reason`/`act`), reduced to
//! their essence: in agentyk the input phase is pure (it is
//! [`crate::turn::TurnState::start`]'s effects), leaving exactly three
//! host-run operations:
//!
//! - [`assemble`] — resolve capabilities into the turn environment (system
//!   prompt + tools). Deterministic per session; durable hosts re-run it on
//!   every activity instead of persisting it.
//! - [`reason`] — one LLM completion (everruns `ReasonAtom`).
//! - [`act`] — one tool execution (everruns `ActAtom`), unknown tools
//!   degrading to an error result the model can react to.
//!
//! Atoms carry no state and emit no events; recording is owned by the state
//! machine's transitions.

use std::collections::HashMap;
use std::sync::Arc;

use crate::capability::{Capability, SystemPromptContext};
use crate::driver::{ChatDriver, ChatRequest, ChatResponse, DeltaSink, ModelSpec};
use crate::error::Result;
use crate::id::SessionId;
use crate::message::{Message, ToolCall};
use crate::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};

/// The resolved per-turn environment: what the model sees and what the host
/// can execute. Rebuilt from the agent value — never persisted.
pub struct AssembledTurn {
    pub system_prompt: Option<String>,
    pub tool_definitions: Vec<ToolDefinition>,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl AssembledTurn {
    pub fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }
}

/// Resolve capabilities into an [`AssembledTurn`]: join the agent's system
/// prompt with capability contributions and collect capability tools
/// (discovering dynamically where needed, e.g. MCP).
pub async fn assemble(
    base_system_prompt: &str,
    capabilities: &[Arc<dyn Capability>],
    session_id: SessionId,
) -> Result<AssembledTurn> {
    let mut tools = HashMap::new();
    let mut tool_definitions = Vec::new();
    for capability in capabilities {
        for tool in capability.tools().await? {
            let definition = tool.definition();
            tools.insert(definition.name.clone(), tool);
            tool_definitions.push(definition);
        }
    }

    let context = SystemPromptContext { session_id };
    let mut parts: Vec<String> = Vec::new();
    if !base_system_prompt.is_empty() {
        parts.push(base_system_prompt.to_string());
    }
    for capability in capabilities {
        if let Some(contribution) = capability.system_prompt_contribution(&context).await {
            parts.push(contribution);
        }
    }
    let system_prompt = if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    };

    Ok(AssembledTurn {
        system_prompt,
        tool_definitions,
        tools,
    })
}

fn chat_request(
    model: &ModelSpec,
    assembled: &AssembledTurn,
    messages: Vec<Message>,
) -> ChatRequest {
    ChatRequest {
        model: model.clone(),
        system_prompt: assembled.system_prompt.clone(),
        messages,
        tools: assembled.tool_definitions.clone(),
    }
}

/// The reason atom: one LLM completion over the current history.
pub async fn reason(
    driver: &dyn ChatDriver,
    model: &ModelSpec,
    assembled: &AssembledTurn,
    messages: Vec<Message>,
) -> Result<ChatResponse> {
    driver
        .complete(chat_request(model, assembled, messages))
        .await
}

/// The reason atom, streaming: text arrives incrementally through `sink` as
/// the driver generates it (see [`DeltaSink`]), and the final response is
/// identical to what [`reason`] would return.
pub async fn reason_streaming(
    driver: &dyn ChatDriver,
    model: &ModelSpec,
    assembled: &AssembledTurn,
    messages: Vec<Message>,
    sink: &mut dyn DeltaSink,
) -> Result<ChatResponse> {
    driver
        .complete_streaming(chat_request(model, assembled, messages), sink)
        .await
}

/// The act atom: execute one tool call. Unknown tools become an error
/// [`ToolOutput`] rather than a host failure — the model gets to recover.
pub async fn act(assembled: &AssembledTurn, call: &ToolCall, context: &ToolContext) -> ToolOutput {
    match assembled.tool(&call.name) {
        Some(tool) => tool.execute(call.arguments.clone(), context).await,
        None => ToolOutput::error(format!("unknown tool `{}`", call.name)),
    }
}
