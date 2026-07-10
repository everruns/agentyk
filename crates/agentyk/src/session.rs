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

use agentyk_core::cancellation::CancellationToken;
use agentyk_core::controls::TurnControls;
use agentyk_core::error::Result;
use agentyk_core::event::Event;
use agentyk_core::event_log::EventLog;
use agentyk_core::executor::{TurnHost, TurnResult};
use agentyk_core::id::SessionId;
use agentyk_core::message::Message;
use agentyk_core::replay::messages_from_events;

use crate::agent::Agent;

/// Everything one [`Session::run_with_options`] call can override. Defaults
/// (`RunOptions::default()`) reproduce plain [`Session::run`]: an
/// uncancellable turn on the agent's default model.
#[derive(Default)]
pub struct RunOptions {
    /// Clone this before calling and cancel the clone from elsewhere to
    /// stop the turn — see [`CancellationToken`].
    pub cancellation: CancellationToken,
    /// Per-turn model/reasoning overrides — see [`TurnControls`].
    pub controls: TurnControls,
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

    /// Run one turn: user input in, final assistant text out. Uncancellable
    /// and on the agent's default model — see [`Session::run_with_options`]
    /// for cancellation and per-turn model/reasoning overrides.
    pub async fn run(&mut self, input: impl Into<String>) -> Result<TurnResult> {
        self.run_with_options(input, RunOptions::default()).await
    }

    /// Run one turn, stoppable from elsewhere: clone `token` before calling,
    /// keep the clone, and call [`CancellationToken::cancel`] on it (e.g.
    /// from a UI event or another task) to stop the turn at its next check
    /// point — between reason/tool steps, or between streaming chunks.
    /// Returns `Err(Error::Cancelled)` if the turn was stopped this way.
    pub async fn run_cancellable(
        &mut self,
        input: impl Into<String>,
        token: CancellationToken,
    ) -> Result<TurnResult> {
        self.run_with_options(
            input,
            RunOptions {
                cancellation: token,
                ..Default::default()
            },
        )
        .await
    }

    /// Run one turn with per-turn model/reasoning overrides, without
    /// rebuilding the agent — see [`TurnControls`].
    pub async fn run_controlled(
        &mut self,
        input: impl Into<String>,
        controls: TurnControls,
    ) -> Result<TurnResult> {
        self.run_with_options(
            input,
            RunOptions {
                controls,
                ..Default::default()
            },
        )
        .await
    }

    /// Run one turn with full control over cancellation and per-turn
    /// overrides — the general form `run`/`run_cancellable`/`run_controlled`
    /// delegate to.
    pub async fn run_with_options(
        &mut self,
        input: impl Into<String>,
        options: RunOptions,
    ) -> Result<TurnResult> {
        let agent = self.agent.inner.clone();
        let executor = agent.executor.clone();
        let effective_model = options.controls.resolve(&agent.model);
        let mut host = TurnHost {
            session_id: self.id,
            system_prompt: &agent.system_prompt,
            model: &effective_model,
            capabilities: &agent.capabilities,
            drivers: &agent.drivers,
            listeners: &agent.listeners,
            max_iterations: agent.max_iterations,
            log: self.log.as_ref(),
            messages: &mut self.messages,
            cancellation: options.cancellation,
            pre_tool_hooks: &agent.pre_tool_hooks,
            post_tool_hooks: &agent.post_tool_hooks,
            budget_checker: agent.budget_checker.clone(),
            context_assembler: agent.context_assembler.as_ref(),
        };
        executor.run_turn(&mut host, Message::user(input)).await
    }
}
