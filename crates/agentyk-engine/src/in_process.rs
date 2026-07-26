//! The in-process host for the canonical turn engine.

use std::sync::Arc;

use agentyk_core::atoms;
use agentyk_core::cancellation::CancellationToken;
use agentyk_core::driver::DeltaSink;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::{EventData, EventListener, EventRequest};
use agentyk_core::id::{EventId, MessageId, SessionId, TurnId};
use agentyk_core::message::Message;
use agentyk_core::tool::{ToolContext, ToolProgress, ToolProgressSink};
use agentyk_core::turn::{TurnOutcome, TurnState};
use async_trait::async_trait;
use chrono::Utc;

use crate::engine::{TurnEngine, TurnOperation};
use crate::host::{TurnHost, TurnResult};

/// Turns a running tool's progress reports into ephemeral `tool.progress`
/// events for the session's listeners.
///
/// It fans out to listeners directly rather than going through
/// [`TurnHost::record`], because the host is exclusively borrowed by the turn
/// while the tool runs. That is sound precisely because the event is
/// ephemeral: nothing is appended to the log and nothing is folded into
/// history, so there is no ordering or sequencing to coordinate — the same
/// reasoning that lets streaming deltas bypass storage.
struct ListenerProgressSink {
    listeners: Vec<Arc<dyn EventListener>>,
    session_id: SessionId,
    turn_id: TurnId,
    call_id: String,
    name: String,
}

#[async_trait]
impl ToolProgressSink for ListenerProgressSink {
    async fn progress(&self, progress: ToolProgress) {
        if self.listeners.is_empty() {
            return;
        }
        let data = EventData::ToolProgress {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            progress,
        };
        let event = EventRequest::with_turn(self.session_id, self.turn_id, data)
            .into_ephemeral_event(EventId::new(), Utc::now());
        for listener in &self.listeners {
            listener.on_event(&event).await;
        }
    }
}

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
                                    .with_extensions(host.environment.extensions.clone())
                                    .with_cancellation(host.cancellation.clone())
                                    .with_progress(Arc::new(ListenerProgressSink {
                                        listeners: host.environment.listeners.clone(),
                                        session_id: host.session_id,
                                        turn_id,
                                        call_id: prepared.call.id.clone(),
                                        name: prepared.call.name.clone(),
                                    }));
                                // Race the call against cancellation instead of
                                // waiting it out: losing means the tool future is
                                // dropped where it stood, which is what kills a
                                // `kill_on_drop` child process. Without this a
                                // cancelled turn still blocks on a long build.
                                let cancellation = host.cancellation.clone();
                                let executed = cancellation
                                    .run_until_cancelled(atoms::act(
                                        &assembled,
                                        &prepared.call,
                                        &context,
                                    ))
                                    .await;
                                match executed {
                                    Some(output) => output,
                                    None => {
                                        let events = state.on_cancel();
                                        host.record(turn_id, events).await?;
                                        return Err(Error::Cancelled);
                                    }
                                }
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
