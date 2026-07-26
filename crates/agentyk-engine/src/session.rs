//! Session — a conversation with an agent.
//!
//! `Session::run` executes one turn of the everruns `input → reason → act`
//! contract through the canonical engine and its in-process host. Every step
//! is appended to the event log
//! and fanned out to listeners; a session can be
//! [resumed](crate::Agent::resume_session) from its log alone.

use std::sync::Arc;

use agentyk_core::cancellation::CancellationToken;
use agentyk_core::capability::{CommandContext, CommandDescriptor};
use agentyk_core::controls::TurnControls;
use agentyk_core::error::Result;
use agentyk_core::event::Event;
use agentyk_core::event_log::EventLog;
use agentyk_core::id::SessionId;
use agentyk_core::input::InputQueue;
use agentyk_core::message::Message;
use agentyk_core::replay::History;
use agentyk_core::tool::ToolOutput;

use crate::agent::Agent;
use crate::host::{TurnHost, TurnResult};
use crate::in_process::InProcessExecutor;

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
    /// Defaults: uncancellable, on the agent's own model.
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

/// A conversation with an agent.
///
/// Holds the session id, the event log, and the message history — which is a
/// projection of that log, never a separate source. Sessions are created from
/// an [`Agent`] and can be resumed from a log alone.
pub struct Session {
    agent: Agent,
    id: SessionId,
    log: Arc<dyn EventLog>,
    history: History,
    input: InputQueue,
}

impl Session {
    pub(crate) fn new(agent: Agent, log: Arc<dyn EventLog>) -> Self {
        Self {
            agent,
            id: SessionId::new(),
            log,
            history: History::new(),
            input: InputQueue::new(),
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
            input: InputQueue::new(),
        })
    }

    /// The session's correlation id — generated internally, readable on
    /// demand, never an input you must mint.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// A handle for steering a turn that is already running.
    ///
    /// `run` borrows the session for the whole turn, so this is taken
    /// *before* starting one and used from wherever the input comes from — a
    /// UI task, a socket, a signal handler. Messages join the conversation at
    /// the turn's next reasoning step; see
    /// [`agentyk_core::input::InputQueue::drain`] for why that boundary.
    ///
    /// Anything pushed while no turn is running waits for the next one, so a
    /// UI never has to know whether the agent is busy.
    ///
    /// ```no_run
    /// # use agentyk::{Agent, Message, Result};
    /// # async fn demo(agent: Agent) -> Result<()> {
    /// let mut session = agent.session();
    /// let steering = session.input();
    ///
    /// tokio::spawn(async move {
    ///     steering.push(Message::user("skip the tests, just build"));
    /// });
    ///
    /// session.run("fix the failing test and verify").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn input(&self) -> InputQueue {
        self.input.clone()
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
            .definition()
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
        for capability in &self.agent.definition().capabilities {
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
        let definition = self.agent.definition();
        let effective_model = options.controls.resolve(&definition.model);
        let mut host = TurnHost::new(
            self.id,
            definition,
            self.agent.environment(),
            self.log.as_ref(),
            &mut self.history,
        )
        .model(&effective_model)
        .cancellation(options.cancellation)
        .input(self.input.clone());
        InProcessExecutor::new()
            .run_turn(&mut host, Message::user(input))
            .await
    }
}
