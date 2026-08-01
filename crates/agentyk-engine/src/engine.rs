//! Canonical one-step turn semantics shared by every execution host.

use agentyk_core::atoms::{self, AssembledTurn};
use agentyk_core::budget::BudgetDecision;
use agentyk_core::context::ContextRequest;
use agentyk_core::driver::{ChatRequest, ChatResponse};
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::hook::{HookContext, HookEvent};
use agentyk_core::message::{Message, ToolCall};
use agentyk_core::middleware::{self, ToolChainOutcome};
use agentyk_core::tool::{
    ToolContext, ToolEventPresentation, ToolNarrationContext, ToolNarrationPhase, ToolOutput,
};
use agentyk_core::turn::{SealReason, TurnAction, TurnOutcome, TurnState};

use crate::hooks::{self, PromptDecision, ToolDecision};
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

/// Canonical effects of opening a turn, before any model work is scheduled.
pub struct PreparedTurnStart {
    /// New pure turn state.
    pub state: TurnState,
    /// `turn.started`, the final (possibly rewritten) input, and hook effects.
    pub events: Vec<EventData>,
    /// Why a prompt hook rejected the turn, if it did.
    pub rejection: Option<HookRejection>,
}

/// A block decision returned by a prompt hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRejection {
    /// Diagnostic reason supplied by the hook.
    pub reason: String,
    /// Optional safer text for a user-facing surface.
    pub user_message: Option<String>,
}

/// The single interpreter of Agentyk turn semantics.
#[derive(Debug, Default, Clone, Copy)]
pub struct TurnEngine;

impl TurnEngine {
    /// Open a turn and apply `user_prompt_submit` hooks before its input is
    /// persisted.
    ///
    /// Durable and immediate hosts both call this method, so prompt mutation
    /// and rejection cannot drift between execution strategies.
    pub async fn start(&self, host: &TurnHost<'_>, input: Message) -> PreparedTurnStart {
        let (mut state, mut events) =
            TurnState::start(host.session_id, host.definition.max_iterations, &input);
        let (decision, hook_events) = hooks::run_prompt_hooks(
            &host.definition.hooks,
            HookContext::turn(host.session_id, state.turn_id),
            input,
        )
        .await;
        events.extend(hook_events);
        let rejection = match decision {
            PromptDecision::Continue(message) => {
                for event in &mut events {
                    if let EventData::InputMessage { message: recorded } = event {
                        *recorded = (*message).clone();
                    }
                }
                None
            }
            PromptDecision::Block {
                reason,
                user_message,
            } => {
                events.extend(
                    state.on_failure(user_message.clone().unwrap_or_else(|| reason.clone())),
                );
                Some(HookRejection {
                    reason,
                    user_message,
                })
            }
        };
        PreparedTurnStart {
            state,
            events,
            rejection,
        }
    }

    /// Apply advisory `turn_end` hooks to a host-provided terminal summary.
    ///
    /// `data` uses the public hook wire shape (`success` plus result/error
    /// details). The engine owns dispatch and error policy; a durable host
    /// owns when its terminal operation has been committed and therefore when
    /// to call this method. Like tool execution, a hook may run more than once
    /// if the durable host crashes before committing its effects.
    pub async fn finish(
        &self,
        host: &TurnHost<'_>,
        turn_id: agentyk_core::id::TurnId,
        data: serde_json::Value,
    ) -> Vec<EventData> {
        hooks::run_advisory_hooks(
            &host.definition.hooks,
            HookEvent::TurnEnd,
            HookContext::turn(host.session_id, turn_id),
            data,
        )
        .await
    }

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
                // Steering lands here and nowhere else: a message wedged
                // between a tool call and its result would make the exchange
                // invalid for every provider, and a reasoning step is the
                // first moment the model could act on it anyway. The drained
                // messages are recorded as ordinary `input.message` events, so
                // they enter history — and a replay — like any other input.
                let steering = host.input.drain();
                let mut events: Vec<EventData> = steering
                    .iter()
                    .cloned()
                    .map(|message| EventData::InputMessage { message })
                    .collect();
                events.extend(state.on_reason_started(Some(&host.model.model)));

                // The host records this step's events *after* prepare returns,
                // so `host.history` does not know about the steering yet. It
                // has to be in the request it is meant to steer, so it is
                // appended here — the events above then bring history to the
                // same place, and a replay produces exactly this list.
                let mut history = host.history.messages().to_vec();
                history.extend(steering);

                let point = host.log.head(host.session_id).await?;
                let context = host
                    .definition
                    .context_assembler
                    .assemble(ContextRequest {
                        point,
                        turn_id: state.turn_id,
                        iteration: state.iterations + 1,
                        model: host.model,
                        token_limit: host.definition.context_token_limit,
                        messages: &history,
                        events: host.log,
                    })
                    .await?;
                let (messages, context_events) = context.into_messages_and_events();
                events.extend(context_events);
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
                // Middleware sees the turn's cancellation signal, so an
                // approval prompt (or any other await inside `before_tool`)
                // can stop waiting when the turn is cancelled.
                let context = ToolContext::new(host.session_id, state.turn_id)
                    .with_extensions(host.environment.extensions.clone())
                    .with_cancellation(host.cancellation.clone());
                let mut events = Vec::new();
                let mut prepared = Vec::with_capacity(calls.len());
                for call in calls {
                    let hook_context = HookContext::turn(host.session_id, state.turn_id);
                    let (hook_decision, hook_events) = hooks::run_pre_tool_hooks(
                        &host.definition.hooks,
                        hook_context,
                        call.clone(),
                    )
                    .await;
                    events.extend(hook_events);
                    let call_after_hooks = match hook_decision {
                        ToolDecision::Block {
                            call: blocked,
                            reason,
                            user_message,
                        } => {
                            if blocked != call {
                                events.extend(state.on_tool_rewritten(
                                    &call.id,
                                    blocked.clone(),
                                    Some("user_hook".into()),
                                ));
                            }
                            events.extend(state.on_tool_started_with_presentation(
                                &blocked.id,
                                tool_presentation(
                                    assembled,
                                    &blocked,
                                    ToolNarrationPhase::Started,
                                    &context,
                                ),
                            ));
                            events.push(EventData::ToolDenied {
                                call_id: blocked.id.clone(),
                                name: blocked.name.clone(),
                                reason: reason.clone(),
                            });
                            if let Some(user_message) = user_message {
                                events.push(EventData::custom(
                                    "hook.user_message",
                                    serde_json::json!({
                                        "call_id": blocked.id,
                                        "message": user_message,
                                    }),
                                ));
                            }
                            prepared.push(PreparedToolCall {
                                call: blocked,
                                denied: Some(ToolOutput::error(reason)),
                            });
                            continue;
                        }
                        ToolDecision::Continue(updated) => updated,
                    };
                    if call_after_hooks != call {
                        events.extend(state.on_tool_rewritten(
                            &call.id,
                            call_after_hooks.clone(),
                            Some("user_hook".into()),
                        ));
                    }

                    let definition = assembled
                        .tool(&call_after_hooks.name)
                        .map(|tool| tool.definition());
                    let decision = middleware::before_tool_chain(
                        &host.definition.middleware,
                        &call_after_hooks,
                        definition.as_ref(),
                        &context,
                    )
                    .await;
                    match decision {
                        ToolChainOutcome::Deny { reason } => {
                            events.extend(state.on_tool_started_with_presentation(
                                &call_after_hooks.id,
                                tool_presentation(
                                    assembled,
                                    &call_after_hooks,
                                    ToolNarrationPhase::Started,
                                    &context,
                                ),
                            ));
                            events.push(EventData::ToolDenied {
                                call_id: call_after_hooks.id.clone(),
                                name: call_after_hooks.name.clone(),
                                reason: reason.clone(),
                            });
                            prepared.push(PreparedToolCall {
                                call: call_after_hooks,
                                denied: Some(ToolOutput::error(reason)),
                            });
                        }
                        ToolChainOutcome::Proceed {
                            call: executed,
                            rewritten,
                        } => {
                            if rewritten {
                                events.extend(state.on_tool_rewritten(
                                    &call_after_hooks.id,
                                    executed.clone(),
                                    None,
                                ));
                            }
                            events.extend(state.on_tool_started_with_presentation(
                                &executed.id,
                                tool_presentation(
                                    assembled,
                                    &executed,
                                    ToolNarrationPhase::Started,
                                    &context,
                                ),
                            ));
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
        let context = ToolContext::new(host.session_id, state.turn_id)
            .with_extensions(host.environment.extensions.clone())
            .with_cancellation(host.cancellation.clone());
        let (output, mut events) = if denied {
            (output, Vec::new())
        } else {
            let definition = assembled.tool(&call.name).map(|tool| tool.definition());
            let output = middleware::after_tool_chain(
                &host.definition.middleware,
                call,
                definition.as_ref(),
                &context,
                output,
            )
            .await;
            let (output, warnings) = hooks::run_post_tool_hooks(
                &host.definition.hooks,
                HookContext::turn(host.session_id, state.turn_id),
                call,
                output,
            )
            .await;
            (output, warnings)
        };
        let phase = if output.is_error {
            ToolNarrationPhase::Failed
        } else {
            ToolNarrationPhase::Completed
        };
        events.extend(state.on_tool_completed_with_presentation(
            &call.id,
            &output,
            tool_presentation(assembled, call, phase, &context),
        ));
        events
    }
}

fn tool_presentation(
    assembled: &AssembledTurn,
    call: &ToolCall,
    phase: ToolNarrationPhase,
    context: &ToolContext,
) -> ToolEventPresentation {
    let Some(tool) = assembled.tool(&call.name) else {
        return ToolEventPresentation::default();
    };
    ToolEventPresentation::new(
        tool.display_name().map(str::to_owned),
        tool.narrate(
            call,
            phase,
            ToolNarrationContext::from_tool_context(context),
        ),
    )
}
