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
use agentyk_core::capability::{CommandContext, CommandDescriptor};
use agentyk_core::controls::TurnControls;
use agentyk_core::error::Result;
use agentyk_core::event::Event;
use agentyk_core::event_log::EventLog;
use agentyk_core::executor::{TurnHost, TurnResult};
use agentyk_core::id::SessionId;
use agentyk_core::message::Message;
use agentyk_core::replay::History;
use agentyk_core::tool::ToolOutput;

use crate::agent::Agent;

/// Everything one [`Session::run_with_options`] call can override. Defaults
/// (`RunOptions::default()`) reproduce plain [`Session::run`]: an
/// uncancellable turn on the agent's default model.
#[derive(Default)]
#[non_exhaustive]
pub struct RunOptions {
    /// Clone this before calling and cancel the clone from elsewhere to
    /// stop the turn — see [`CancellationToken`].
    pub cancellation: CancellationToken,
    /// Per-turn model/reasoning overrides — see [`TurnControls`].
    pub controls: TurnControls,
}

impl RunOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop the turn from elsewhere — see [`CancellationToken`].
    pub fn cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = token;
        self
    }

    /// Per-turn model/reasoning overrides — see [`TurnControls`].
    pub fn controls(mut self, controls: TurnControls) -> Self {
        self.controls = controls;
        self
    }
}

pub struct Session {
    agent: Agent,
    id: SessionId,
    log: Arc<dyn EventLog>,
    history: History,
}

impl Session {
    pub(crate) fn new(agent: Agent, log: Arc<dyn EventLog>) -> Self {
        Self {
            agent,
            id: SessionId::new(),
            log,
            history: History::new(),
        }
    }

    pub(crate) async fn resume(
        agent: Agent,
        log: Arc<dyn EventLog>,
        session_id: SessionId,
    ) -> Result<Self> {
        let events = log.read(session_id).await?;
        Ok(Self {
            agent,
            id: session_id,
            log,
            history: History::from_events(&events),
        })
    }

    /// The session's correlation id — generated internally, readable on
    /// demand, never an input you must mint.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The reconstructed-or-live message history — always equal to a replay
    /// of this session's log.
    pub fn messages(&self) -> &[Message] {
        self.history.messages()
    }

    /// All events recorded for this session, ordered by sequence.
    pub async fn events(&self) -> Result<Vec<Event>> {
        self.log.read(self.id).await
    }

    /// Every slash command the agent's capabilities expose — see
    /// [`agentyk_core::capability::Capability::commands`].
    pub fn commands(&self) -> Vec<CommandDescriptor> {
        self.agent
            .config
            .capabilities
            .iter()
            .flat_map(|capability| capability.commands())
            .collect()
    }

    /// Run a command directly — bypasses the turn loop entirely (no model
    /// call, no event log entry): the first capability whose
    /// [`agentyk_core::capability::Capability::commands`] lists `name` and
    /// whose `execute_command` returns `Some(..)` wins. Errors with an
    /// "unknown command" message if none claims it.
    pub async fn execute_command(&self, name: &str, args: &str) -> Result<ToolOutput> {
        let context = CommandContext::new(self.id);
        for capability in &self.agent.config.capabilities {
            if capability.commands().iter().any(|c| c.name == name)
                && let Some(output) = capability.execute_command(name, args, &context).await
            {
                return Ok(output);
            }
        }
        Ok(ToolOutput::error(format!("unknown command `/{name}`")))
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
        self.run_with_options(input, RunOptions::new().cancellation(token))
            .await
    }

    /// Run one turn with per-turn model/reasoning overrides, without
    /// rebuilding the agent — see [`TurnControls`].
    pub async fn run_controlled(
        &mut self,
        input: impl Into<String>,
        controls: TurnControls,
    ) -> Result<TurnResult> {
        self.run_with_options(input, RunOptions::new().controls(controls))
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
        let config = self.agent.config.clone();
        let executor = self.agent.executor.clone();
        let effective_model = options.controls.resolve(config.model());
        // Composition travels as one value; only the genuinely per-run parts
        // are set here. Adding a composition knob does not touch this call.
        let mut host = TurnHost::new(self.id, &config, self.log.as_ref(), &mut self.history)
            .model(&effective_model)
            .cancellation(options.cancellation);
        executor.run_turn(&mut host, Message::user(input)).await
    }
}
