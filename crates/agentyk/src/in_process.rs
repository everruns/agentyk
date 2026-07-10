//! The default execution strategy: drive the turn state machine to
//! completion in a single async call.

use agentyk_core::atoms;
use agentyk_core::budget::BudgetDecision;
use agentyk_core::cancellation::CancellationToken;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::hooks::PreToolUseDecision;
use agentyk_core::id::TurnId;
use agentyk_core::message::Message;
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
    cancellation: CancellationToken,
}

#[async_trait]
impl agentyk_core::driver::DeltaSink for RecordingDeltaSink<'_, '_> {
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let effects = vec![EventData::OutputMessageDelta {
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
            // Checked once per action — between reason/tool steps — so a
            // cancellation request takes effect at the next natural
            // boundary rather than mid-call.
            if host.cancellation.is_cancelled() {
                let effects = state.on_cancel();
                host.record(turn_id, effects).await?;
                return Err(Error::Cancelled);
            }
            if let Some(checker) = &host.budget_checker
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

                    let messages = host
                        .context_assembler
                        .assemble(host.session_id, host.messages)
                        .await;
                    let cancellation = host.cancellation.clone();
                    let mut sink = RecordingDeltaSink {
                        host: &mut *host,
                        turn_id,
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
                    let effects = state.on_tool_started(&call.id);
                    host.record(turn_id, effects).await?;
                    let context = ToolContext {
                        session_id: host.session_id,
                        turn_id,
                    };

                    let mut denial: Option<String> = None;
                    for hook in host.pre_tool_hooks {
                        if let PreToolUseDecision::Deny { reason } =
                            hook.before_tool_use(&call, &context).await
                        {
                            denial = Some(reason);
                            break;
                        }
                    }

                    let output = if let Some(reason) = &denial {
                        host.record(
                            turn_id,
                            vec![EventData::ToolDenied {
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                reason: reason.clone(),
                            }],
                        )
                        .await?;
                        ToolOutput::error(reason.clone())
                    } else {
                        let mut output = atoms::act(&assembled, &call, &context).await;
                        for hook in host.post_tool_hooks {
                            output = hook.after_tool_exec(&call, output, &context).await;
                        }
                        output
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
