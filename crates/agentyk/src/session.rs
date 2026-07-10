//! Session — a conversation with an agent.
//!
//! `Session::run` executes one turn of the everruns `input → reason → act`
//! contract by delegating to the agent's
//! [`TurnExecutor`](agentyk_core::executor::TurnExecutor) (default:
//! [`crate::in_process::InProcessExecutor`]), which drives the
//! [`TurnState`](agentyk_core::turn::TurnState) machine over the
//! [`atoms`](agentyk_core::atoms). Every step is appended to the event log
//! and fanned out to listeners; a session can be
//! [resumed](crate::Agent::resume_session) from its log alone.

use std::sync::Arc;

use agentyk_core::error::Result;
use agentyk_core::event::Event;
use agentyk_core::event_log::EventLog;
use agentyk_core::executor::{TurnHost, TurnResult};
use agentyk_core::id::SessionId;
use agentyk_core::message::Message;
use agentyk_core::replay::messages_from_events;

use crate::agent::Agent;

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
