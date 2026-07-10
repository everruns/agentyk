//! Session — a conversation with an agent.
//!
//! `Session::run` executes one turn of the everruns `input → reason → act`
//! contract by delegating to the agent's [`crate::executor::TurnExecutor`]
//! (default: [`crate::executor::InProcessExecutor`]), which drives the
//! [`crate::turn::TurnState`] machine over the [`crate::atoms`]. Every step
//! is appended to the [`EventLog`] and fanned out to listeners; a session can
//! be [resumed](crate::Agent::resume_session) from its log alone.

use std::sync::Arc;

use crate::agent::Agent;
use crate::driver::Usage;
use crate::error::Result;
use crate::event::{Event, EventData};
use crate::event_log::EventLog;
use crate::executor::TurnHost;
use crate::id::{SessionId, TurnId};
use crate::message::Message;

/// The outcome of one [`Session::run`] call.
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

pub struct Session {
    agent: Agent,
    id: SessionId,
    log: Arc<dyn EventLog>,
    messages: Vec<Message>,
}

impl Session {
    pub(crate) fn new(agent: Agent, log: Arc<dyn EventLog>) -> Self {
        Self {
            agent,
            id: SessionId::new(),
            log,
            messages: Vec::new(),
        }
    }

    pub(crate) async fn resume(
        agent: Agent,
        log: Arc<dyn EventLog>,
        session_id: SessionId,
    ) -> Result<Self> {
        let events = log.read(session_id).await?;
        let messages = messages_from_events(&events);
        Ok(Self {
            agent,
            id: session_id,
            log,
            messages,
        })
    }

    /// The session's correlation id — generated internally, readable on
    /// demand, never an input you must mint.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The reconstructed-or-live message history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// All events recorded for this session, ordered by sequence.
    pub async fn events(&self) -> Result<Vec<Event>> {
        self.log.read(self.id).await
    }

    /// Run one turn: user input in, final assistant text out.
    pub async fn run(&mut self, input: impl Into<String>) -> Result<TurnResult> {
        let agent = self.agent.inner.clone();
        let executor = agent.executor.clone();
        let mut host = TurnHost {
            session_id: self.id,
            system_prompt: &agent.system_prompt,
            model: &agent.model,
            capabilities: &agent.capabilities,
            drivers: &agent.drivers,
            listeners: &agent.listeners,
            max_iterations: agent.max_iterations,
            log: self.log.as_ref(),
            messages: &mut self.messages,
        };
        executor.run_turn(&mut host, Message::user(input)).await
    }
}

/// The message a recorded event contributes to history, if any. History is a
/// pure fold of this over the event log — the invariant that makes
/// resume-from-log (including mid-turn, for durable hosts) possible.
pub(crate) fn message_from_event_data(data: &EventData) -> Option<Message> {
    match data {
        EventData::InputMessage { message } | EventData::OutputMessage { message } => {
            Some(message.clone())
        }
        EventData::ToolCompleted {
            call_id, output, ..
        } => Some(Message::tool_result(call_id.clone(), output.clone())),
        _ => None,
    }
}

/// Rebuild message history from an event log — the replay half of the
/// persistence story.
pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|e| e.sequence);
    ordered
        .into_iter()
        .filter_map(|event| message_from_event_data(&event.data))
        .collect()
}
