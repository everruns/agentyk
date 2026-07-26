//! The in-process host for the canonical turn engine.

use std::sync::Arc;

use agentyk_core::atoms;
use agentyk_core::cancellation::CancellationToken;
use agentyk_core::concurrency::join_all;
use agentyk_core::driver::DeltaSink;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::{EventData, EventListener, EventRequest};
use agentyk_core::id::{EventId, MessageId, SessionId, TurnId};
use agentyk_core::message::Message;
use agentyk_core::tool::{ToolContext, ToolOutput, ToolProgress, ToolProgressSink};
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
///
/// A prepared tool batch runs **concurrently** by default: when a model asks
/// for three file reads at once it means all three, and running them one after
/// another turns a model's parallelism into a host's latency. Results are
/// still recorded in batch order, so the event sequence is identical either
/// way — see [`InProcessExecutor::sequential`] for when to give that up.
#[derive(Debug, Clone, Copy)]
pub struct InProcessExecutor {
    concurrent: bool,
}

impl Default for InProcessExecutor {
    fn default() -> Self {
        Self { concurrent: true }
    }
}

impl InProcessExecutor {
    /// The default: a prepared batch runs concurrently.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run a batch one call at a time.
    ///
    /// The one thing this buys is policy *inside* a batch: budget checks and
    /// cancellation are evaluated between calls, so a batch of ten can stop
    /// after two. Concurrent dispatch necessarily commits to the whole batch
    /// (cancellation still drops all of it at once). Choose this when tools
    /// contend for the same resource, or when a budget must bite mid-batch.
    pub fn sequential() -> Self {
        Self { concurrent: false }
    }
}

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
                    // Execute the batch, then record it. Denied calls resolve
                    // instantly; the rest run concurrently when the host is
                    // configured for it, which is what makes a model's
                    // parallel reads actually parallel.
                    let mut pending = Vec::new();
                    let mut outputs: Vec<Option<ToolOutput>> = Vec::with_capacity(calls.len());
                    for prepared in &calls {
                        match &prepared.denied {
                            Some(output) => outputs.push(Some(output.clone())),
                            None => {
                                outputs.push(None);
                                pending.push(prepared.call.clone());
                            }
                        }
                    }

                    if !pending.is_empty() {
                        let contexts: Vec<ToolContext> = pending
                            .iter()
                            .map(|call| {
                                ToolContext::new(host.session_id, turn_id)
                                    .with_extensions(host.environment.extensions.clone())
                                    .with_cancellation(host.cancellation.clone())
                                    .with_progress(Arc::new(ListenerProgressSink {
                                        listeners: host.environment.listeners.clone(),
                                        session_id: host.session_id,
                                        turn_id,
                                        call_id: call.id.clone(),
                                        name: call.name.clone(),
                                    })
                                        as Arc<dyn ToolProgressSink>)
                            })
                            .collect();

                        // Race the whole batch against cancellation rather
                        // than waiting it out: losing drops every in-flight
                        // tool future where it stands, which is what kills a
                        // `kill_on_drop` child process.
                        let cancellation = host.cancellation.clone();
                        let executed = match self.concurrent {
                            true => {
                                let futures = pending
                                    .iter()
                                    .zip(&contexts)
                                    .map(|(call, context)| atoms::act(&assembled, call, context));
                                cancellation.run_until_cancelled(join_all(futures)).await
                            }
                            // Sequential dispatch still checks policy between
                            // calls, which a concurrent batch cannot do — see
                            // `InProcessExecutor::sequential`.
                            false => {
                                cancellation
                                    .run_until_cancelled(async {
                                        let mut results = Vec::with_capacity(pending.len());
                                        for (call, context) in pending.iter().zip(&contexts) {
                                            results
                                                .push(atoms::act(&assembled, call, context).await);
                                        }
                                        results
                                    })
                                    .await
                            }
                        };
                        let Some(results) = executed else {
                            let events = state.on_cancel();
                            host.record(turn_id, events).await?;
                            return Err(Error::Cancelled);
                        };
                        let mut results = results.into_iter();
                        for slot in outputs.iter_mut() {
                            if slot.is_none() {
                                *slot = results.next();
                            }
                        }
                    }

                    // Recording is sequential and in batch order, however the
                    // calls ran: two replays of one session must see the same
                    // event sequence.
                    for (prepared, output) in calls.into_iter().zip(outputs) {
                        let output = output.expect("every prepared call produced an output");
                        let events = engine
                            .complete_tool(
                                host,
                                &assembled,
                                &mut state,
                                &prepared.call,
                                output,
                                prepared.denied.is_some(),
                            )
                            .await;
                        host.record(turn_id, events).await?;
                        if let Some(step) = engine.guard(host, &mut state).await {
                            host.record(turn_id, step.events).await?;
                            if let TurnOperation::Finished(outcome) = step.operation {
                                return turn_result(host, state, outcome);
                            }
                            unreachable!("an engine guard only returns terminal operations");
                        }
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
