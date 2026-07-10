//! Turn execution strategies.
//!
//! [`TurnExecutor`] is the seam that decides *how* a turn runs — the agentyk
//! analogue of everruns-runtime's host adapter / turn-strategy layer. The
//! machine ([`crate::turn::TurnState`]) and the atoms ([`crate::atoms`]) are
//! the shared vocabulary; an executor is just a way of driving them:
//!
//! - [`InProcessExecutor`] (the default) drives the machine to completion in
//!   a single async call.
//! - A durable host (everruns' engine) implements this trait by scheduling
//!   each [`crate::turn::TurnAction`] as a retryable activity, checkpointing
//!   the serialized [`crate::turn::TurnState`] between steps, appending the
//!   returned effects to its event store transactionally, and rebuilding
//!   history from the log on resume. No part of the loop below is hidden
//!   from it — the machine and atoms are public API.

use async_trait::async_trait;

use crate::atoms;
use crate::capability::Capability;
use crate::driver::{DriverRegistry, ModelSpec};
use crate::error::{Error, Result};
use crate::event::{EventData, EventListener, EventRequest};
use crate::event_log::EventLog;
use crate::id::SessionId;
use crate::message::Message;
use crate::session::{TurnResult, message_from_event_data};
use crate::tool::ToolContext;
use crate::turn::{TurnAction, TurnOutcome, TurnState};
use std::sync::Arc;

/// Everything a turn needs from its host: the agent's composition and the
/// session's log and history. Executors receive it per turn.
pub struct TurnHost<'a> {
    pub session_id: SessionId,
    pub system_prompt: &'a str,
    pub model: &'a ModelSpec,
    pub capabilities: &'a [Arc<dyn Capability>],
    pub drivers: &'a DriverRegistry,
    pub listeners: &'a [Arc<dyn EventListener>],
    pub max_iterations: usize,
    pub log: &'a dyn EventLog,
    /// Live message history; the executor keeps it in sync with the events
    /// it records.
    pub messages: &'a mut Vec<Message>,
}

impl TurnHost<'_> {
    /// Record effects: append each to the event log, fan out to listeners,
    /// and update the history projection. This is the single write path —
    /// history stays a pure fold over the log.
    pub async fn record(
        &mut self,
        turn_id: crate::id::TurnId,
        effects: Vec<EventData>,
    ) -> Result<()> {
        for data in effects {
            if let Some(message) = message_from_event_data(&data) {
                self.messages.push(message);
            }
            let event = self
                .log
                .append(EventRequest::with_turn(self.session_id, turn_id, data))
                .await?;
            for listener in self.listeners {
                listener.on_event(&event).await;
            }
        }
        Ok(())
    }
}

/// A strategy for executing one agent turn.
#[async_trait]
pub trait TurnExecutor: Send + Sync {
    async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult>;
}

/// The default executor: drive the state machine to completion in-process.
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
