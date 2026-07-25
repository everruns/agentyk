//! The in-process host for the canonical turn engine.

use agentyk_core::atoms;
use agentyk_core::cancellation::CancellationToken;
use agentyk_core::driver::DeltaSink;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::id::{MessageId, TurnId};
use agentyk_core::message::Message;
use agentyk_core::tool::ToolContext;
use agentyk_core::turn::{TurnOutcome, TurnState};
use async_trait::async_trait;

use crate::engine::{TurnEngine, TurnOperation};
use crate::host::{TurnHost, TurnResult};

struct RecordingDeltaSink<'h, 'a> {
    host: &'h mut TurnHost<'a>,
    turn_id: TurnId,
    message_id: MessageId,
    cancellation: CancellationToken,
}

#[async_trait]
impl DeltaSink for RecordingDeltaSink<'_, '_> {
    async fn delta(&mut self, delta: &str, accumulated: &str) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.host
            .record(
                self.turn_id,
                vec![EventData::OutputMessageDelta {
                    message_id: self.message_id,
                    delta: delta.to_string(),
                    accumulated: accumulated.to_string(),
                }],
            )
            .await
    }
}

/// Executes every operation from [`TurnEngine`] immediately in the caller's
/// async task.
#[derive(Debug, Default, Clone, Copy)]
pub struct InProcessExecutor;

impl InProcessExecutor {
    /// Run one turn by immediately executing every prepared engine operation.
    pub async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult> {
        let engine = TurnEngine;
        let assembled = engine.assemble(host).await?;
        let driver = host
            .environment
            .drivers
            .get(&host.model.driver)
            .ok_or_else(|| Error::UnknownDriver(host.model.driver.to_string()))?;
        let (mut state, events) =
            TurnState::start(host.session_id, host.definition.max_iterations, &input);
        let turn_id = state.turn_id;
        host.record(turn_id, events).await?;

        loop {
            let step = engine.prepare(host, &assembled, &mut state).await?;
            host.record(turn_id, step.events).await?;
            match step.operation {
                TurnOperation::InvokeModel { request } => {
                    let cancellation = host.cancellation.clone();
                    let message_id = state.current_message_id.unwrap_or_default();
                    let mut sink = RecordingDeltaSink {
                        host: &mut *host,
                        turn_id,
                        message_id,
                        cancellation,
                    };
                    match driver.complete_streaming(request, &mut sink).await {
                        Ok(response) => {
                            let events = engine.complete_model(&mut state, &response);
                            host.record(turn_id, events).await?;
                        }
                        Err(Error::Cancelled) => {
                            let events = state.on_cancel();
                            host.record(turn_id, events).await?;
                            return Err(Error::Cancelled);
                        }
                        Err(error) => {
                            let events = engine.fail_model(&mut state, &error);
                            host.record(turn_id, events).await?;
                            return Err(error);
                        }
                    }
                }
                TurnOperation::InvokeTools { calls } => {
                    for (index, prepared) in calls.into_iter().enumerate() {
                        if index > 0
                            && let Some(step) = engine.guard(host, &mut state).await
                        {
                            host.record(turn_id, step.events).await?;
                            if let TurnOperation::Finished(outcome) = step.operation {
                                return turn_result(host, state, outcome);
                            }
                            unreachable!("an engine guard only returns terminal operations");
                        }
                        let was_denied = prepared.denied.is_some();
                        let output = match prepared.denied {
                            Some(output) => output,
                            None => {
                                let context = ToolContext::new(host.session_id, turn_id)
                                    .with_extensions(host.environment.extensions.clone());
                                atoms::act(&assembled, &prepared.call, &context).await
                            }
                        };
                        let events = engine
                            .complete_tool(
                                host,
                                &assembled,
                                &mut state,
                                &prepared.call,
                                output,
                                was_denied,
                            )
                            .await;
                        host.record(turn_id, events).await?;
                    }
                }
                TurnOperation::Finished(outcome) => {
                    return turn_result(host, state, outcome);
                }
            }
        }
    }
}

fn turn_result(host: &TurnHost<'_>, state: TurnState, outcome: TurnOutcome) -> Result<TurnResult> {
    match outcome {
        TurnOutcome::Success { response } => Ok(TurnResult::new(
            state.turn_id,
            response,
            state.iterations,
            state.tool_calls_executed,
            state.usage,
        )),
        TurnOutcome::MaxIterations => Err(Error::MaxIterations(host.definition.max_iterations)),
        TurnOutcome::Failed { error } => Err(Error::Other(error)),
        TurnOutcome::Cancelled => Err(Error::Cancelled),
        TurnOutcome::Sealed(reason) => Err(Error::Sealed(reason)),
    }
}
