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
    /// The agent's prompt joined with every capability's contribution, or
    /// `None` when all of them are empty.
    pub system_prompt: Option<String>,
    /// What the model is offered this turn. Deferred tools are executable
    /// but deliberately absent — see [`crate::tool::DeferrablePolicy`].
    pub tool_definitions: Vec<ToolDefinition>,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl AssembledTurn {
    /// Look up an executable tool by name, including deferred ones that
    /// were never offered to the model.
    pub fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }
}

/// Resolve capabilities into an [`AssembledTurn`]: join the agent's system
/// prompt with capability contributions and collect capability tools
/// (discovering dynamically where needed, e.g. MCP). A tool whose
/// [`crate::tool::ToolPolicy::deferrable`] is `Deferred` stays executable
/// (it's still in the lookup table) but is left out of the definitions sent
/// to the model — see [`crate::tool::DeferrablePolicy`].
pub async fn assemble(
    base_system_prompt: &str,
    capabilities: &[Arc<dyn Capability>],
    session_id: SessionId,
) -> Result<AssembledTurn> {
    use crate::tool::DeferrablePolicy;

    let mut tools = HashMap::new();
    let mut tool_definitions = Vec::new();
    for capability in capabilities {
        for tool in capability.tools().await? {
            let definition = tool.definition();
            if tool.policy().deferrable == DeferrablePolicy::Never {
                tool_definitions.push(definition.clone());
            }
            tools.insert(definition.name.clone(), tool);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{DeferrablePolicy, ToolPolicy};
    use serde_json::json;

    /// A minimal, dependency-free executor for these tests — core has no
    /// async runtime, and the fake `Tool`/`Capability` impls below never
    /// actually suspend, so a bare poll loop resolves on the first poll.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }

    struct FixedTool {
        name: &'static str,
        policy: ToolPolicy,
    }

    #[async_trait::async_trait]
    impl Tool for FixedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(self.name, "", json!({"type": "object"}))
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _context: &ToolContext,
        ) -> ToolOutput {
            ToolOutput::text(self.name)
        }

        fn policy(&self) -> ToolPolicy {
            self.policy
        }
    }

    struct FixedCapability {
        tools: Vec<Arc<dyn Tool>>,
    }

    #[async_trait::async_trait]
    impl Capability for FixedCapability {
        fn id(&self) -> &str {
            "fixed"
        }

        async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
            Ok(self.tools.clone())
        }
    }

    #[test]
    fn deferred_tools_are_executable_but_not_offered_to_the_model() {
        let capability: Arc<dyn Capability> = Arc::new(FixedCapability {
            tools: vec![
                Arc::new(FixedTool {
                    name: "visible",
                    policy: ToolPolicy::default(),
                }),
                Arc::new(FixedTool {
                    name: "hidden",
                    policy: ToolPolicy {
                        deferrable: DeferrablePolicy::Deferred,
                    },
                }),
            ],
        });

        let assembled = block_on(assemble("", &[capability], SessionId::new())).unwrap();

        let offered: Vec<&str> = assembled
            .tool_definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(offered, vec!["visible"]);

        // Still callable directly, e.g. by a future discovery mechanism.
        assert!(assembled.tool("hidden").is_some());
        assert!(assembled.tool("visible").is_some());
    }
}
