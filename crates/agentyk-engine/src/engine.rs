//! Canonical one-step turn semantics shared by every execution host.

use agentyk_core::atoms::{self, AssembledTurn};
use agentyk_core::budget::BudgetDecision;
use agentyk_core::driver::{ChatRequest, ChatResponse};
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::message::ToolCall;
use agentyk_core::middleware::{self, ToolChainOutcome};
use agentyk_core::tool::{ToolContext, ToolOutput};
use agentyk_core::turn::{SealReason, TurnAction, TurnOutcome, TurnState};

use crate::host::TurnHost;
/// External work requested by one prepared engine step.
///
/// Operations are runtime values, not durable payloads. In particular, a
/// model request may contain credentials. Persist the accompanying events and
/// resolve protected execution resources at the host's activity boundary.
#[derive(Debug, Clone)]
pub enum TurnOperation {
    /// Ask the configured model for the next assistant message.
    InvokeModel {
        /// Complete provider-neutral request.
        request: ChatRequest,
    },
    /// Run a prepared tool batch. A host may dispatch runnable calls
    /// sequentially, concurrently, or as durable activities.
    InvokeTools {
        /// Calls after middleware has rewritten or denied each one.
        calls: Vec<PreparedToolCall>,
    },
    /// The turn has reached a terminal outcome.
    Finished(TurnOutcome),
}

/// One call in a prepared tool batch.
#[derive(Debug, Clone)]
pub struct PreparedToolCall {
    /// Call after every middleware rewrite.
    pub call: ToolCall,
    /// Present when middleware denied the call and no tool may run.
    pub denied: Option<ToolOutput>,
}

/// One engine decision: persist `events`, then execute `operation`.
#[derive(Debug, Clone)]
pub struct PreparedStep {
    /// Durable transition events that precede the operation.
    pub events: Vec<EventData>,
    /// Work the host must execute or schedule.
    pub operation: TurnOperation,
}

/// The single interpreter of Agentyk turn semantics.
#[derive(Debug, Default, Clone, Copy)]
pub struct TurnEngine;

impl TurnEngine {
    /// Resolve the agent's capabilities into the environment used by steps.
    pub async fn assemble(&self, host: &TurnHost<'_>) -> Result<AssembledTurn> {
        atoms::assemble(
            host.definition.instructions.as_str(),
            &host.definition.capabilities,
            host.session_id,
        )
        .await
    }

    /// Prepare exactly one operation from the current state.
    ///
    /// A durable host persists the returned events before scheduling the
    /// operation. The in-process host records them and executes immediately.
    pub async fn prepare(
        &self,
        host: &TurnHost<'_>,
        assembled: &AssembledTurn,
        state: &mut TurnState,
    ) -> Result<PreparedStep> {
        if let Some(step) = self.guard(host, state).await {
            return Ok(step);
        }

        match state.next_action() {
            TurnAction::Reason => {
                let events = state.on_reason_started(Some(&host.model.model));
                let messages = host
                    .definition
                    .context_assembler
                    .assemble(host.session_id, host.history.messages())
                    .await;
                Ok(PreparedStep {
                    events,
                    operation: TurnOperation::InvokeModel {
                        request: atoms::chat_request(host.model, assembled, messages),
                    },
                })
            }
            TurnAction::ExecuteTool { call } => {
                let pending = state.pending_tool_actions();
                let calls = if pending.is_empty() {
                    vec![call]
                } else {
                    pending
                        .into_iter()
                        .filter_map(|action| match action {
                            TurnAction::ExecuteTool { call } => Some(call),
                            _ => None,
                        })
                        .collect()
                };
                let context = ToolContext::new(host.session_id, state.turn_id)
                    .with_extensions(host.environment.extensions.clone());
                let mut events = Vec::new();
                let mut prepared = Vec::with_capacity(calls.len());
                for call in calls {
                    let definition = assembled.tool(&call.name).map(|tool| tool.definition());
                    let decision = middleware::before_tool_chain(
                        &host.definition.middleware,
                        &call,
                        definition.as_ref(),
                        &context,
                    )
                    .await;
                    match decision {
                        ToolChainOutcome::Deny { reason } => {
                            events.extend(state.on_tool_started(&call.id));
                            events.push(EventData::ToolDenied {
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                reason: reason.clone(),
                            });
                            prepared.push(PreparedToolCall {
                                call,
                                denied: Some(ToolOutput::error(reason)),
                            });
                        }
                        ToolChainOutcome::Proceed {
                            call: executed,
                            rewritten,
                        } => {
                            if rewritten {
                                events.extend(state.on_tool_rewritten(
                                    &call.id,
                                    executed.clone(),
                                    None,
                                ));
                            }
                            events.extend(state.on_tool_started(&executed.id));
                            prepared.push(PreparedToolCall {
                                call: executed,
                                denied: None,
                            });
                        }
                    }
                }
                Ok(PreparedStep {
                    events,
                    operation: TurnOperation::InvokeTools { calls: prepared },
                })
            }
            TurnAction::Complete(outcome) => Ok(PreparedStep {
                events: Vec::new(),
                operation: TurnOperation::Finished(outcome),
            }),
        }
    }

    /// Apply cancellation and budget policy before one external action.
    ///
    /// Hosts that execute a prepared tool batch one call at a time use this
    /// between calls so policy remains responsive inside the batch.
    pub async fn guard(&self, host: &TurnHost<'_>, state: &mut TurnState) -> Option<PreparedStep> {
        if host.cancellation.is_cancelled() {
            return Some(PreparedStep {
                events: state.on_cancel(),
                operation: TurnOperation::Finished(TurnOutcome::Cancelled),
            });
        }
        if let Some(checker) = &host.definition.budget_checker
            && checker.check(host.session_id).await == BudgetDecision::Seal
        {
            let reason = SealReason::BudgetExhausted;
            return Some(PreparedStep {
                events: state.on_seal(reason),
                operation: TurnOperation::Finished(TurnOutcome::Sealed(reason)),
            });
        }
        None
    }

    /// Apply a successful model operation and return its durable events.
    pub fn complete_model(&self, state: &mut TurnState, response: &ChatResponse) -> Vec<EventData> {
        state.on_reason_completed(response)
    }

    /// Apply a failed model operation and return its durable event.
    pub fn fail_model(&self, state: &mut TurnState, error: &Error) -> Vec<EventData> {
        state.on_failure(error.to_string())
    }

    /// Apply one tool result, including post-tool middleware when the tool
    /// actually ran, and return its durable completion event.
    pub async fn complete_tool(
        &self,
        host: &TurnHost<'_>,
        assembled: &AssembledTurn,
        state: &mut TurnState,
        call: &ToolCall,
        output: ToolOutput,
        denied: bool,
    ) -> Vec<EventData> {
        let output = if denied {
            output
        } else {
            let context = ToolContext::new(host.session_id, state.turn_id)
                .with_extensions(host.environment.extensions.clone());
            let definition = assembled.tool(&call.name).map(|tool| tool.definition());
            middleware::after_tool_chain(
                &host.definition.middleware,
                call,
                definition.as_ref(),
                &context,
                output,
            )
            .await
        };
        state.on_tool_completed(&call.id, &output)
    }
}
