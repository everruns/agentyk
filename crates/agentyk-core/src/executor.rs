//! Turn execution — the strategy seam.
//!
//! [`TurnExecutor`] decides *how* a turn runs — the agentyk analogue of
//! everruns-runtime's host adapter / turn-strategy layer. The machine
//! ([`crate::turn::TurnState`]) and the atoms ([`crate::atoms`]) are the
//! shared vocabulary; an executor is a way of driving them:
//!
//! - `agentyk`'s `InProcessExecutor` (the default) drives the machine to
//!   completion in a single async call.
//! - A durable host (everruns' engine) implements this trait by scheduling
//!   each [`crate::turn::TurnAction`] as a retryable activity, checkpointing
//!   the serialized [`crate::turn::TurnState`] between steps, appending the
//!   returned effects to its event store transactionally, and rebuilding
//!   history from the log on resume.

use async_trait::async_trait;
use chrono::Utc;

use crate::capability::Capability;
use crate::driver::{DriverRegistry, ModelSpec, Usage};
use crate::error::Result;
use crate::event::{EventData, EventListener, EventRequest};
use crate::event_log::EventLog;
use crate::id::{EventId, SessionId, TurnId};
use crate::message::Message;
use crate::replay::message_from_event_data;
use std::sync::Arc;

/// The outcome of one executed turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub turn_id: TurnId,
    /// The model's final text response.
    pub response: String,
    /// LLM completions performed within the turn.
    pub iterations: usize,
    /// Tool calls executed within the turn.
    pub tool_calls: usize,
    pub usage: Usage,
}

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
    /// Record effects: durable ones are appended to the event log and fold
    /// into the history projection; ephemeral ones (see
    /// [`EventData::is_ephemeral`]) skip the log entirely and go straight to
    /// listeners with `sequence: None`. Either way, every listener sees
    /// every event — this is the single write path, and history stays a
    /// pure fold over only the durable events.
    pub async fn record(&mut self, turn_id: TurnId, effects: Vec<EventData>) -> Result<()> {
        for data in effects {
            let request = EventRequest::with_turn(self.session_id, turn_id, data);
            let event = if request.data.is_ephemeral() {
                request.into_ephemeral_event(EventId::new(), Utc::now())
            } else {
                if let Some(message) = message_from_event_data(&request.data) {
                    self.messages.push(message);
                }
                self.log.append(request).await?
            };
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
