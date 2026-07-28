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
use agentyk_core::event::{Event, EventData, EventRequest, notify_event_listeners};
use agentyk_core::event_log::{EventLog, ExpectedVersion, SessionPoint};
use agentyk_core::hook::{HookContext, HookEvent};
use agentyk_core::id::SessionId;
use agentyk_core::input::InputQueue;
use agentyk_core::message::Message;
use agentyk_core::replay::History;
use agentyk_core::tool::ToolOutput;
use agentyk_core::turn::TurnState;

use crate::agent::Agent;
use crate::hooks;
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
    started: bool,
    closed: bool,
}

/// Read-only projection of a session at an immutable historical point.
#[derive(Debug, Clone)]
pub struct SessionView {
    point: SessionPoint,
    events: Vec<Event>,
    history: History,
}

impl SessionView {
    /// Exact event position this view includes.
    pub fn point(&self) -> SessionPoint {
        self.point
    }

    /// Durable events through [`Self::point`], in sequence order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Message projection at this point.
    pub fn messages(&self) -> &[Message] {
        self.history.messages()
    }
}

impl Session {
    pub(crate) fn new(agent: Agent, log: Arc<dyn EventLog>) -> Self {
        Self {
            agent,
            id: SessionId::new(),
            log,
            history: History::new(),
            input: InputQueue::new(),
            started: false,
            closed: false,
        }
    }

    pub(crate) async fn resume(
        agent: Agent,
        log: Arc<dyn EventLog>,
        session_id: SessionId,
    ) -> Result<Self> {
        let events = log.read(session_id).await?;
        let started = !events.is_empty();
        Ok(Self {
            agent,
            id: session_id,
            log,
            history: History::from_events(&events),
            input: InputQueue::new(),
            // A non-empty log proves the first turn boundary was crossed.
            // An empty recovered session has no durable evidence that its
            // start hook ran, so it follows the ordinary first-turn path.
            started,
            closed: false,
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
    /// ```
    /// use agentyk_core::{input::InputQueue, message::Message};
    ///
    /// // What `session.input()` hands out: a queue whose clones share one
    /// // backlog, so a UI task can push while the turn runs.
    /// let steering = InputQueue::new();
    /// let from_the_ui = steering.clone();
    /// from_the_ui.push(Message::user("skip the tests, just build"));
    ///
    /// assert_eq!(steering.drain().len(), 1);
    /// ```
    pub fn input(&self) -> InputQueue {
        self.input.clone()
    }

    /// Whether [`Session::close`] has fired this session's end hooks.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close this in-memory session and fire advisory `session_end` hooks.
    ///
    /// Dropping a Rust value cannot await async hooks, so session end is
    /// explicit. Closing is idempotent. A closed session retains its history
    /// for inspection but refuses new turns.
    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let warnings = hooks::run_advisory_hooks(
            &self.agent.definition().hooks,
            HookEvent::SessionEnd,
            HookContext::session(self.id),
            serde_json::json!({ "reason": "closed" }),
        )
        .await;
        self.record_session_events(warnings).await?;
        self.closed = true;
        Ok(())
    }

    /// Current immutable point at the session's durable head.
    pub async fn point(&self) -> Result<SessionPoint> {
        self.log.head(self.id).await
    }

    /// Reconstruct a read-only view without executing or modifying history.
    pub async fn inspect(&self, point: SessionPoint) -> Result<SessionView> {
        if point.session_id != self.id {
            return Err(agentyk_core::error::Error::EventLog(format!(
                "point belongs to {}, not session {}",
                point.session_id, self.id
            )));
        }
        let head = self.log.head(self.id).await?;
        if point.sequence > head.sequence {
            return Err(agentyk_core::error::Error::EventLog(format!(
                "point {} is past session head {}",
                point.sequence, head.sequence
            )));
        }
        let events = self.log.read_through(point).await?;
        Ok(SessionView {
            point,
            history: History::from_events(&events),
            events,
        })
    }

    /// Create an independent branch from a completed-turn boundary.
    ///
    /// The original session remains unchanged. The returned session inherits
    /// history through `point` and records `session.forked` lineage before new
    /// turns are appended.
    pub async fn fork(&self, point: SessionPoint) -> Result<Self> {
        let view = self.inspect(point).await?;
        if !is_completed_turn_boundary(view.events()) {
            return Err(agentyk_core::error::Error::EventLog(
                "sessions can only fork from an empty stream or a completed-turn boundary".into(),
            ));
        }
        let child_id = self.log.fork(point).await?;
        let mut child = Self::resume(self.agent.clone(), self.log.clone(), child_id).await?;
        child.started = false;
        Ok(child)
    }

    /// Continue the newest incomplete turn at this session's current head.
    ///
    /// Returns `None` when the session has no incomplete turn. Recovery is
    /// at-least-once for external actions; see
    /// [`InProcessExecutor::resume_turn`]. The agent's default model is used.
    pub async fn resume_pending(&mut self) -> Result<Option<TurnResult>> {
        self.resume_pending_with_options(RunOptions::default())
            .await
    }

    /// Continue the newest incomplete turn with explicit cancellation and
    /// model controls.
    pub async fn resume_pending_with_options(
        &mut self,
        options: RunOptions,
    ) -> Result<Option<TurnResult>> {
        let events = self.log.read(self.id).await?;
        let Some(turn_id) = events.iter().rev().find_map(|event| {
            matches!(event.data, EventData::TurnStarted { .. })
                .then_some(event.turn_id)
                .flatten()
        }) else {
            return Ok(None);
        };
        let state = TurnState::replay(&events, turn_id)?;
        if state.is_complete() {
            return Ok(None);
        }

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
        .cancellation(options.cancellation);
        InProcessExecutor::new()
            .resume_turn(&mut host, state)
            .await
            .map(Some)
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
    ///
    /// Takes anything that becomes a [`Message`], so `run("hello")` stays a
    /// one-liner while a turn opened with an image is
    /// `run(Message::user_multimodal(parts))` rather than a different method.
    pub async fn run(&mut self, input: impl Into<Message>) -> Result<TurnResult> {
        self.run_with_options(input, RunOptions::default()).await
    }

    /// Run one turn, stoppable from elsewhere: clone `token` before calling,
    /// keep the clone, and call [`CancellationToken::cancel`] on it (e.g.
    /// from a UI event or another task) to stop the turn at its next check
    /// point — between reason/tool steps, or between streaming chunks.
    /// Returns `Err(Error::Cancelled)` if the turn was stopped this way.
    pub async fn run_cancellable(
        &mut self,
        input: impl Into<Message>,
        token: CancellationToken,
    ) -> Result<TurnResult> {
        self.run_with_options(input, RunOptions::new().cancellation(token))
            .await
    }

    /// Run one turn with per-turn model/reasoning overrides, without
    /// rebuilding the agent — see [`TurnControls`].
    pub async fn run_controlled(
        &mut self,
        input: impl Into<Message>,
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
        input: impl Into<Message>,
        options: RunOptions,
    ) -> Result<TurnResult> {
        if self.closed {
            return Err(agentyk_core::error::Error::Other(
                "session is closed".into(),
            ));
        }
        if !self.started {
            let warnings = hooks::run_advisory_hooks(
                &self.agent.definition().hooks,
                HookEvent::SessionStart,
                HookContext::session(self.id),
                serde_json::json!({ "agent": self.agent.name() }),
            )
            .await;
            self.record_session_events(warnings).await?;
            self.started = true;
        }
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
            .run_turn(&mut host, input.into())
            .await
    }

    async fn record_session_events(&mut self, events: Vec<EventData>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let requests = events
            .into_iter()
            .map(|data| EventRequest::new(self.id, data))
            .collect();
        let version = self.log.head(self.id).await?.sequence;
        let recorded = self
            .log
            .append_batch(self.id, ExpectedVersion::Exact(version), requests)
            .await?;
        for event in recorded {
            self.history.apply(&event.data);
            notify_event_listeners(&self.agent.environment().listeners, &event).await;
        }
        Ok(())
    }
}

fn is_completed_turn_boundary(events: &[Event]) -> bool {
    let Some(turn_id) = events.iter().rev().find_map(|event| event.turn_id) else {
        return true;
    };
    events.iter().any(|event| {
        event.turn_id == Some(turn_id)
            && matches!(
                event.data,
                EventData::TurnCompleted { .. }
                    | EventData::TurnFailed { .. }
                    | EventData::TurnCancelled
                    | EventData::TurnSealed { .. }
            )
    })
}
