//! The default execution strategy: drive the turn state machine to
//! completion in a single async call.

use agentyk_core::atoms;
use agentyk_core::error::{Error, Result};
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::message::Message;
use agentyk_core::tool::ToolContext;
use agentyk_core::turn::{TurnAction, TurnOutcome, TurnState};
use async_trait::async_trait;

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
            match state.next_action() {
                TurnAction::Reason => {
                    match atoms::reason(driver.as_ref(), &model, &assembled, host.messages.clone())
                        .await
                    {
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
                    let effects = state.on_tool_started();
                    host.record(turn_id, effects).await?;
                    let context = ToolContext {
                        session_id: host.session_id,
                        turn_id,
                    };
                    let output = atoms::act(&assembled, &call, &context).await;
                    let effects = state.on_tool_completed(&output);
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
                    };
                }
            }
        }
    }
}
