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

use crate::cancellation::CancellationToken;
use crate::config::AgentConfig;
use crate::driver::{ModelSpec, Usage};
use crate::error::Result;
use crate::event::{EventData, EventRequest};
use crate::event_log::EventLog;
use crate::id::{EventId, SessionId, TurnId};
use crate::message::Message;
use crate::replay::History;

/// The outcome of one executed turn.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TurnResult {
    /// Correlates with every event this turn recorded.
    pub turn_id: TurnId,
    /// The model's final text response.
    pub response: String,
    /// LLM completions performed within the turn.
    pub iterations: usize,
    /// Tool calls executed within the turn.
    pub tool_calls: usize,
    /// Tokens spent across every completion in the turn.
    pub usage: Usage,
}

impl TurnResult {
    /// Record how a turn ended.
    pub fn new(
        turn_id: TurnId,
        response: impl Into<String>,
        iterations: usize,
        tool_calls: usize,
        usage: Usage,
    ) -> Self {
        Self {
            turn_id,
            response: response.into(),
            iterations,
            tool_calls,
            usage,
        }
    }
}

/// Everything a turn needs from its host: the agent's composition
/// ([`AgentConfig`]) plus what is specific to *this* run — the session's log
/// and history, the effective model, and cancellation.
///
/// The split matters: composition knobs are added to `AgentConfig` and reach
/// every executor for free, so this struct does not grow a field (and break
/// third-party executors) every time the agent gains a capability.
#[non_exhaustive]
pub struct TurnHost<'a> {
    /// The session this turn belongs to.
    pub session_id: SessionId,
    /// The agent's composition — prompt, capabilities, drivers, hooks, seams.
    pub config: &'a AgentConfig,
    /// The model for *this* turn: the agent's default with any per-turn
    /// [`crate::controls::TurnControls`] applied. Not `config.model()`.
    pub model: &'a ModelSpec,
    /// Where durable events are appended. Reach it through
    /// [`TurnHost::record`] rather than directly, so history and listeners
    /// stay in step.
    pub log: &'a dyn EventLog,
    /// The session's message history. Advanced by [`TurnHost::record`] from
    /// the events themselves — see [`History`] for why it is not a plain
    /// `Vec<Message>`.
    pub history: &'a mut History,
    /// Checked between turn actions (and, for streaming drivers, between
    /// chunks) so a caller can stop a running turn — see
    /// [`CancellationToken`].
    pub cancellation: CancellationToken,
}

impl<'a> TurnHost<'a> {
    /// The effective model defaults to the agent's own; use
    /// [`TurnHost::model`] to override it for one turn.
    pub fn new(
        session_id: SessionId,
        config: &'a AgentConfig,
        log: &'a dyn EventLog,
        history: &'a mut History,
    ) -> Self {
        Self {
            session_id,
            config,
            model: config.model(),
            log,
            history,
            cancellation: CancellationToken::new(),
        }
    }

    /// Run this turn on a different model than the agent's default — what
    /// [`crate::controls::TurnControls`] resolves to.
    pub fn model(mut self, model: &'a ModelSpec) -> Self {
        self.model = model;
        self
    }

    /// Let this turn be stopped from elsewhere.
    pub fn cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = token;
        self
    }
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
                self.history.apply(&request.data);
                self.log.append(request).await?
            };
            for listener in &self.config.listeners {
                listener.on_event(&event).await;
            }
        }
        Ok(())
    }
}

/// A strategy for executing one agent turn.
#[async_trait]
pub trait TurnExecutor: Send + Sync {
    /// Drive one turn to completion.
    ///
    /// The implementation owns the loop: start a [`crate::turn::TurnState`],
    /// run the atoms it asks for, and record every transition's effects via
    /// [`TurnHost::record`]. What it must *not* invent is policy —
    /// middleware, budget and cancellation come from the host and should be
    /// honored as the built-in executor does.
    async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult>;
}
