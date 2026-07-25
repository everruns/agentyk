//! The default execution strategy: drive the turn state machine to
//! completion in a single async call.

use agentyk_core::atoms;
use agentyk_core::budget::BudgetDecision;
use agentyk_core::cancellation::CancellationToken;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::id::{MessageId, TurnId};
use agentyk_core::message::Message;
use agentyk_core::middleware::{self, ToolChainOutcome};
use agentyk_core::tool::{ToolContext, ToolOutput};
use agentyk_core::turn::{SealReason, TurnAction, TurnOutcome, TurnState};
use async_trait::async_trait;

/// Forwards streaming deltas straight to [`TurnHost::record`] as ephemeral
/// `output.message.delta` events. Checks cancellation once per chunk, so a
/// caller can stop a turn mid-stream rather than waiting for a whole
/// completion — see [`Session::run_cancellable`](crate::Session::run_cancellable).
struct RecordingDeltaSink<'h, 'a> {
    host: &'h mut TurnHost<'a>,
    turn_id: TurnId,
    /// Correlates deltas with the started/completed events of the same
    /// assistant message — set from `TurnState::current_message_id`.
    message_id: MessageId,
    cancellation: CancellationToken,
}

#[async_trait]
impl agentyk_core::driver::DeltaSink for RecordingDeltaSink<'_, '_> {
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let effects = vec![EventData::OutputMessageDelta {
            message_id: self.message_id,
            delta: delta.to_string(),
            accumulated: accumulated.to_string(),
        }];
        self.host.record(self.turn_id, effects).await
    }
}

/// Drives [`TurnState`] over the [`atoms`] in-process. The reference
/// implementation every other executor (durable, custom) must stay
/// behaviorally aligned with.
#[derive(Debug, Default, Clone, Copy)]
pub struct InProcessExecutor;

#[async_trait]
impl TurnExecutor for InProcessExecutor {
    async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult> {
        let assembled = atoms::assemble(
            host.config.system_prompt.as_str(),
            &host.config.capabilities,
            host.session_id,
        )
        .await?;
        let driver = host
            .config
            .drivers
            .get(&host.model.driver)
            .ok_or_else(|| Error::UnknownDriver(host.model.driver.to_string()))?;
        let model = host.model.clone();

        let (mut state, effects) =
            TurnState::start(host.session_id, host.config.max_iterations, &input);
        let turn_id = state.turn_id;
        host.record(turn_id, effects).await?;

        loop {
            // Checked once per action — between reason/tool steps — so a
            // cancellation request takes effect at the next natural
            // boundary rather than mid-call.
            if host.cancellation.is_cancelled() {
                let effects = state.on_cancel();
                host.record(turn_id, effects).await?;
                return Err(Error::Cancelled);
            }
            if let Some(checker) = &host.config.budget_checker
                && checker.check(host.session_id).await == BudgetDecision::Seal
            {
                let effects = state.on_seal(SealReason::BudgetExhausted);
                host.record(turn_id, effects).await?;
                return Err(Error::Sealed(SealReason::BudgetExhausted));
            }

            match state.next_action() {
                TurnAction::Reason => {
                    let started = state.on_reason_started(Some(&model.model));
                    host.record(turn_id, started).await?;
                    // `on_reason_started` just set this; reuse it so deltas
                    // and the completed event share the started event's id.
                    let message_id = state.current_message_id.unwrap_or_default();

                    let messages = host
                        .config
                        .context_assembler
                        .assemble(host.session_id, host.messages)
                        .await;
                    let cancellation = host.cancellation.clone();
                    let mut sink = RecordingDeltaSink {
                        host: &mut *host,
                        turn_id,
                        message_id,
                        cancellation,
                    };
                    let outcome = atoms::reason_streaming(
                        driver.as_ref(),
                        &model,
                        &assembled,
                        messages,
                        &mut sink,
                    )
                    .await;

                    match outcome {
                        Ok(response) => {
                            let effects = state.on_reason_completed(&response);
                            host.record(turn_id, effects).await?;
                        }
                        Err(Error::Cancelled) => {
                            let effects = state.on_cancel();
                            host.record(turn_id, effects).await?;
                            return Err(Error::Cancelled);
                        }
                        Err(error) => {
                            let effects = state.on_failure(error.to_string());
                            host.record(turn_id, effects).await?;
                            return Err(error);
                        }
                    }
                }
                TurnAction::ExecuteTool { call } => {
                    let context = ToolContext::new(host.session_id, turn_id)
                        .with_extensions(host.config.extensions.clone());
                    let definition = assembled.tool(&call.name).map(|tool| tool.definition());

                    // The middleware chain resolves first, so `tool.started`
                    // records the call as it will actually run — a rewritten
                    // call is never announced under its original arguments.
                    let outcome = middleware::before_tool_chain(
                        &host.config.middleware,
                        &call,
                        definition.as_ref(),
                        &context,
                    )
                    .await;

                    let (executed, output) = match outcome {
                        ToolChainOutcome::Deny { reason } => {
                            let effects = state.on_tool_started(&call.id);
                            host.record(turn_id, effects).await?;
                            host.record(
                                turn_id,
                                vec![EventData::ToolDenied {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    reason: reason.clone(),
                                }],
                            )
                            .await?;
                            (call.clone(), ToolOutput::error(reason))
                        }
                        ToolChainOutcome::Proceed {
                            call: executed,
                            rewritten,
                        } => {
                            if rewritten {
                                // Into the state, not just the event stream:
                                // a resumed host must run the rewrite.
                                let effects =
                                    state.on_tool_rewritten(&executed.id, executed.clone(), None);
                                host.record(turn_id, effects).await?;
                            }
                            let effects = state.on_tool_started(&executed.id);
                            host.record(turn_id, effects).await?;
                            let output = atoms::act(&assembled, &executed, &context).await;
                            let output = middleware::after_tool_chain(
                                &host.config.middleware,
                                &executed,
                                definition.as_ref(),
                                &context,
                                output,
                            )
                            .await;
                            (executed, output)
                        }
                    };

                    let effects = state.on_tool_completed(&executed.id, &output);
                    host.record(turn_id, effects).await?;
                }
                TurnAction::Complete(outcome) => {
                    return match outcome {
                        TurnOutcome::Success { response } => Ok(TurnResult::new(
                            turn_id,
                            response,
                            state.iterations,
                            state.tool_calls_executed,
                            state.usage,
                        )),
                        TurnOutcome::MaxIterations => {
                            Err(Error::MaxIterations(host.config.max_iterations))
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
