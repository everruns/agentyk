//! A satellite [`TurnExecutor`] that adds everruns-style **hint-based tool
//! approval** — behavior agentyk-core's built-in executor doesn't have, built
//! entirely over core's public seams (`atoms` + [`TurnState`] + [`TurnHost`]).
//!
//! This is the proof for `docs/extensibility.md`: gap 4 (mutating / approval /
//! capability-contributed guardrails) lives in the executor layer, not in
//! core's hook trait. Note in particular [`ApprovalDecision::Deny`] carries a
//! `user_message` — a richer decision than core's `PreToolUseDecision`, owned
//! here rather than forced into the contract.
//!
//! It is intentionally minimal (non-streaming, no budget seam) — enough to
//! demonstrate the seam, not a full re-implementation of `InProcessExecutor`.

use agentyk_core::atoms;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::message::{Message, ToolCall};
use agentyk_core::tool::{ToolContext, ToolOutput};
use agentyk_core::turn::{TurnAction, TurnOutcome, TurnState};
use async_trait::async_trait;
use std::sync::Arc;

use crate::hints::ToolHints;

/// What an [`Approver`] decides for a risky tool call. `Deny` carries a
/// `user_message` distinct from any internal reason — the shape everruns
/// guardrails want and core's `PreToolUseDecision` lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny { user_message: String },
}

/// Consulted before a tool whose [`ToolHints`] say it
/// [`needs_approval`](ToolHints::needs_approval) runs. A host implements this
/// to pause for a human, apply a policy, etc.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, call: &ToolCall, hints: &ToolHints) -> ApprovalDecision;
}

/// An approver that permits everything — the no-op baseline.
pub struct AllowAll;

#[async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// A [`TurnExecutor`] that gates destructive/open-world tools through an
/// [`Approver`], reading each tool's risk from the `ToolDefinition.metadata`
/// hints hatch. Everything else follows the standard reason/act loop.
pub struct EverrunsExecutor {
    approver: Arc<dyn Approver>,
}

impl EverrunsExecutor {
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self {
            approver: Arc::new(approver),
        }
    }
}

impl Default for EverrunsExecutor {
    fn default() -> Self {
        Self::new(AllowAll)
    }
}

#[async_trait]
impl TurnExecutor for EverrunsExecutor {
    async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult> {
        let assembled =
            atoms::assemble(host.system_prompt, host.capabilities, host.session_id).await?;
        let driver = host
            .drivers
            .get(&host.model.driver)
            .ok_or_else(|| Error::UnknownDriver(host.model.driver.to_string()))?;
        let model = host.model.clone();

        let (mut state, effects) = TurnState::start(host.session_id, host.max_iterations, &input);
        let turn_id = state.turn_id;
        host.record(turn_id, effects).await?;

        loop {
            if host.cancellation.is_cancelled() {
                let effects = state.on_cancel();
                host.record(turn_id, effects).await?;
                return Err(Error::Cancelled);
            }

            match state.next_action() {
                TurnAction::Reason => {
                    let started = state.on_reason_started(Some(&model.model));
                    host.record(turn_id, started).await?;
                    let messages = host
                        .context_assembler
                        .assemble(host.session_id, host.messages)
                        .await;
                    match atoms::reason(driver.as_ref(), &model, &assembled, messages).await {
                        Ok(response) => {
                            let effects = state.on_reason_completed(&response);
                            host.record(turn_id, effects).await?;
                        }
                        Err(error) => {
                            let effects = state.on_failure(error.to_string());
                            host.record(turn_id, effects).await?;
                            return Err(error);
                        }
                    }
                }
                TurnAction::ExecuteTool { call } => {
                    let effects = state.on_tool_started(&call.id);
                    host.record(turn_id, effects).await?;
                    let context = ToolContext {
                        session_id: host.session_id,
                        turn_id,
                        extensions: host.extensions.clone(),
                    };

                    // The satellite behavior: classify the tool by its hints
                    // (from the metadata hatch) and gate the risky ones.
                    let hints = assembled
                        .tool(&call.name)
                        .map(|tool| tool.definition())
                        .as_ref()
                        .and_then(ToolHints::from_definition)
                        .unwrap_or_default();

                    let output = if hints.needs_approval() {
                        match self.approver.approve(&call, &hints).await {
                            ApprovalDecision::Allow => {
                                atoms::act(&assembled, &call, &context).await
                            }
                            ApprovalDecision::Deny { user_message } => {
                                host.record(
                                    turn_id,
                                    vec![EventData::ToolDenied {
                                        call_id: call.id.clone(),
                                        name: call.name.clone(),
                                        reason: user_message.clone(),
                                    }],
                                )
                                .await?;
                                ToolOutput::error(user_message)
                            }
                        }
                    } else {
                        atoms::act(&assembled, &call, &context).await
                    };

                    let effects = state.on_tool_completed(&call.id, &output);
                    host.record(turn_id, effects).await?;
                }
                TurnAction::Complete(outcome) => {
                    return match outcome {
                        TurnOutcome::Success { response } => Ok(TurnResult {
                            turn_id,
                            response,
                            iterations: state.iterations,
                            tool_calls: state.tool_calls_executed,
                            usage: state.usage,
                        }),
                        TurnOutcome::MaxIterations => {
                            Err(Error::MaxIterations(host.max_iterations))
                        }
                        TurnOutcome::Failed { error } => Err(Error::Other(error)),
                        TurnOutcome::Cancelled => Err(Error::Cancelled),
                        TurnOutcome::Sealed(reason) => Err(Error::Sealed(reason)),
                    };
                }
            }
        }
    }
}
