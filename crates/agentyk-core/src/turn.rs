//! Turn state machine — the deterministic turn reducer.
//!
//! Adopted from everruns-core's `TurnStateMachine` / `RuntimeTurnState`, with
//! one sharpening: the machine here is **sans-IO**. Transitions are pure —
//! they mutate bookkeeping and return the [`EventData`] to record — and the
//! canonical `agentyk-engine` step engine is the only interpreter that
//! sequences them with external model and tool work.
//!
//! ```text
//! start ──▶ PendingReason ──reason──▶ PendingAct ──act──▶ PendingReason ──▶ …
//!                │                                                │
//!                └────────────▶ Completed(TurnOutcome) ◀──────────┘
//! ```
//!
//! [`TurnState::next_action`] exposes the next semantic action and
//! [`TurnState::pending_tool_actions`] exposes a whole ready tool batch.
//! These are reducer primitives, not a second public orchestration loop:
//! execution hosts use the canonical engine, which applies middleware,
//! policy, and event generation before returning prepared operations.
//!
//! ## Why this supports durable execution
//!
//! - [`TurnState::replay`] reconstructs actionable state from durable events;
//!   serialized state is an optional cache, never an authority.
//! - State carries bookkeeping only — no credentials, tool objects, or
//!   message history.
//! - Transitions are idempotent by tool-call id, so a host can safely
//!   redeliver activity results.
//! - Message history is independently folded from the same event stream by
//!   [`crate::replay::messages_from_events`].

use serde::{Deserialize, Serialize};

use crate::driver::{ChatResponse, Usage};
use crate::error::{Error, Result};
use crate::event::{Event, EventData};
use crate::id::{MessageId, SessionId, TurnId};
use crate::message::{Message, ToolCall};
use crate::tool::ToolOutput;

/// Why a turn was deliberately sealed rather than left to run to
/// `MaxIterations` or fail. Mirrors everruns' `SealReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SealReason {
    /// A durable turn was reclaimed repeatedly without making forward
    /// progress. Host concern — agentyk's in-process executor can't
    /// crash-loop, so it never seals for this reason; a durable host would.
    NoProgress,
    /// A [`crate::budget::BudgetChecker`] returned
    /// [`crate::budget::BudgetDecision::Seal`].
    BudgetExhausted,
}

/// How a turn ended. Mirrors everruns' `TurnOutcome`
/// (Success/Failed/MaxIterations/Sealed) — `Cancelled` is the agentyk
/// analogue of a caller-driven seal via
/// [`crate::cancellation::CancellationToken`]; `Sealed` is a host- or
/// budget-driven one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The model produced a final answer.
    Success {
        /// Its text — what a caller shows the user.
        response: String,
    },
    /// Something went wrong: a driver error, or a host abort.
    Failed {
        /// What happened.
        error: String,
    },
    /// The turn kept asking for tools until it hit the iteration ceiling,
    /// without ever answering.
    MaxIterations,
    /// Stopped cooperatively via a
    /// [`crate::cancellation::CancellationToken`] rather than failing.
    Cancelled,
    /// Stopped deliberately to prevent wasted work — see [`SealReason`].
    Sealed(SealReason),
}

/// One tool call within a reason step's batch, tracked independently so a
/// parallel host can start/complete calls out of order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingCall {
    /// The call to run — the rewritten one, if middleware changed it.
    pub call: ToolCall,
    /// Whether `tool.started` was already recorded for this call — makes
    /// `on_tool_started` idempotent per call id across crash/retry.
    pub started: bool,
    /// Set once the call resolves, whatever ran it.
    pub output: Option<ToolOutput>,
}

/// Current phase of the turn. `PendingAct` carries the whole batch from the
/// triggering reason step so a resumed host — sequential or parallel —
/// knows exactly what remains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TurnPhase {
    /// Waiting on the model.
    PendingReason,
    /// Waiting on tools: the batch the last reason step asked for.
    PendingAct {
        /// Every call in the batch, each tracked independently.
        calls: Vec<PendingCall>,
    },
    /// Finished.
    Completed(TurnOutcome),
}

/// The next thing a host must do. One `TurnAction` maps to one durable
/// activity.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnAction {
    /// Run the LLM (the reason atom) over the current history.
    Reason,
    /// Execute one tool call (the act atom).
    ExecuteTool {
        /// Which call to run.
        call: ToolCall,
    },
    /// The turn is finished.
    Complete(TurnOutcome),
}

/// Serializable turn bookkeeping. Everything else a step needs — model,
/// credentials, assembled tools, message history — is environment the host
/// provides per step; it is deliberately NOT part of this state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TurnState {
    /// The session this turn belongs to.
    pub session_id: SessionId,
    /// This turn's id, minted at [`TurnState::start`].
    pub turn_id: TurnId,
    /// Ceiling on reason steps before the turn gives up.
    pub max_iterations: usize,
    /// Where the turn currently is.
    pub phase: TurnPhase,
    /// Completed reason (LLM) calls.
    pub iterations: usize,
    /// Executed tool calls.
    pub tool_calls_executed: usize,
    /// Tokens spent so far in this turn.
    pub usage: Usage,
    /// The id of the assistant message currently being generated, set by
    /// [`Self::on_reason_started`] and read by the executor (to tag streaming
    /// deltas) and [`Self::on_reason_completed`], so all three
    /// `output.message.*` events of one reason step share a `message_id`.
    /// `None` between reason steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_message_id: Option<MessageId>,
}

impl TurnState {
    /// Reconstruct one turn entirely from its durable event stream.
    ///
    /// This is the durable-host entry point: serialized checkpoints may
    /// cache this reduction, but are never required for correctness.
    pub fn replay(events: &[Event], turn_id: TurnId) -> Result<Self> {
        let mut ordered: Vec<&Event> = events
            .iter()
            .filter(|event| event.turn_id == Some(turn_id))
            .collect();
        ordered.sort_by_key(|event| event.sequence);

        let mut state: Option<Self> = None;
        for event in ordered {
            match &event.data {
                EventData::TurnStarted { max_iterations } => {
                    state = Some(Self {
                        session_id: event.session_id,
                        turn_id,
                        max_iterations: *max_iterations,
                        phase: TurnPhase::PendingReason,
                        iterations: 0,
                        tool_calls_executed: 0,
                        usage: Usage::default(),
                        current_message_id: None,
                    });
                }
                // Ephemeral events (deltas, tool progress) never reach a log,
                // so replay only sees them if a host chose to keep them; either
                // way they carry no turn state.
                EventData::InputMessage { .. }
                | EventData::OutputMessageDelta { .. }
                | EventData::ToolProgress { .. } => {}
                EventData::OutputMessageStarted { message_id, .. } => {
                    replay_state(&mut state, turn_id)?.current_message_id = Some(*message_id);
                }
                EventData::OutputMessageCompleted { message, usage, .. } => {
                    let state = replay_state(&mut state, turn_id)?;
                    state.current_message_id = None;
                    state.iterations += 1;
                    state.usage.add(*usage);
                    if message.tool_calls.is_empty() {
                        state.phase = TurnPhase::Completed(TurnOutcome::Success {
                            response: message.text(),
                        });
                    } else if state.iterations >= state.max_iterations {
                        state.phase = TurnPhase::Completed(TurnOutcome::MaxIterations);
                    } else {
                        state.phase = TurnPhase::PendingAct {
                            calls: message
                                .tool_calls
                                .iter()
                                .cloned()
                                .map(|call| PendingCall {
                                    call,
                                    started: false,
                                    output: None,
                                })
                                .collect(),
                        };
                    }
                }
                EventData::ToolRewritten { call_id, call, .. } => {
                    let call = call.as_ref().ok_or_else(|| {
                        Error::EventLog(format!(
                            "cannot replay legacy tool.rewritten event for call `{call_id}`"
                        ))
                    })?;
                    let state = replay_state(&mut state, turn_id)?;
                    if let TurnPhase::PendingAct { calls } = &mut state.phase
                        && let Some(pending) =
                            calls.iter_mut().find(|pending| pending.call.id == *call_id)
                    {
                        pending.call = call.clone();
                    }
                }
                EventData::ToolStarted { call } => {
                    let state = replay_state(&mut state, turn_id)?;
                    if let TurnPhase::PendingAct { calls } = &mut state.phase
                        && let Some(pending) =
                            calls.iter_mut().find(|pending| pending.call.id == call.id)
                    {
                        pending.call = call.clone();
                        pending.started = true;
                    }
                }
                EventData::ToolCompleted {
                    call_id,
                    output,
                    is_error,
                    metadata,
                    ..
                } => {
                    let state = replay_state(&mut state, turn_id)?;
                    if let TurnPhase::PendingAct { calls } = &mut state.phase
                        && let Some(pending) =
                            calls.iter_mut().find(|pending| pending.call.id == *call_id)
                        && pending.output.is_none()
                    {
                        pending.output = Some(
                            ToolOutput::new(output.clone(), *is_error)
                                .with_metadata(metadata.clone()),
                        );
                        state.tool_calls_executed += 1;
                        if calls.iter().all(|pending| pending.output.is_some()) {
                            state.phase = TurnPhase::PendingReason;
                        }
                    }
                }
                EventData::ToolDenied { .. } => {}
                EventData::TurnCompleted { .. } => {
                    let state = replay_state(&mut state, turn_id)?;
                    if !matches!(
                        state.phase,
                        TurnPhase::Completed(TurnOutcome::Success { .. })
                    ) {
                        return Err(Error::EventLog(format!(
                            "turn {turn_id} completed without a final output message"
                        )));
                    }
                }
                EventData::TurnFailed { error } => {
                    let state = replay_state(&mut state, turn_id)?;
                    if state.iterations >= state.max_iterations {
                        state.phase = TurnPhase::Completed(TurnOutcome::MaxIterations);
                    } else {
                        state.phase = TurnPhase::Completed(TurnOutcome::Failed {
                            error: error.clone(),
                        });
                    }
                }
                EventData::TurnCancelled => {
                    replay_state(&mut state, turn_id)?.phase =
                        TurnPhase::Completed(TurnOutcome::Cancelled);
                }
                EventData::TurnSealed { reason } => {
                    replay_state(&mut state, turn_id)?.phase =
                        TurnPhase::Completed(TurnOutcome::Sealed(*reason));
                }
                EventData::Custom {
                    event_type,
                    state_bearing: true,
                    ..
                } => {
                    return Err(Error::EventLog(format!(
                        "cannot replay unknown state-bearing event `{event_type}`"
                    )));
                }
                EventData::Custom { .. } => {}
            }
        }
        state.ok_or_else(|| Error::EventLog(format!("turn {turn_id} has no turn.started event")))
    }

    /// Begin a turn. Returns the state (in `PendingReason`) and the effects
    /// to record: `turn.started` and `input.message`.
    pub fn start(
        session_id: SessionId,
        max_iterations: usize,
        input: &Message,
    ) -> (Self, Vec<EventData>) {
        let state = Self {
            session_id,
            turn_id: TurnId::new(),
            max_iterations,
            phase: TurnPhase::PendingReason,
            iterations: 0,
            tool_calls_executed: 0,
            usage: Usage::default(),
            current_message_id: None,
        };
        let effects = vec![
            EventData::TurnStarted { max_iterations },
            EventData::InputMessage {
                message: input.clone(),
            },
        ];
        (state, effects)
    }

    /// Pure peek at the next action: the first call in the batch that
    /// hasn't resolved yet, in order. A sequential executor can drive the
    /// whole turn from this alone — see [`Self::pending_tool_actions`] for
    /// the parallel-capable alternative. Idempotent — safe to call again
    /// after a crash without re-recording anything.
    pub fn next_action(&self) -> TurnAction {
        match &self.phase {
            TurnPhase::PendingReason => TurnAction::Reason,
            TurnPhase::PendingAct { calls } => {
                match calls.iter().find(|pending| pending.output.is_none()) {
                    Some(pending) => TurnAction::ExecuteTool {
                        call: pending.call.clone(),
                    },
                    // Unreachable by construction; degrade to another reason step.
                    None => TurnAction::Reason,
                }
            }
            TurnPhase::Completed(outcome) => TurnAction::Complete(outcome.clone()),
        }
    }

    /// Every call in the current batch that hasn't been started yet, all at
    /// once — what a parallel-capable executor fans out concurrently
    /// instead of following `next_action` one call at a time. Empty outside
    /// `PendingAct` or once every call has been started.
    pub fn pending_tool_actions(&self) -> Vec<TurnAction> {
        match &self.phase {
            TurnPhase::PendingAct { calls } => calls
                .iter()
                .filter(|pending| !pending.started)
                .map(|pending| TurnAction::ExecuteTool {
                    call: pending.call.clone(),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// About to run the reason atom for the upcoming iteration. Allocates the
    /// `message_id` for this assistant message (stored in
    /// [`Self::current_message_id`] for the deltas and the completed event to
    /// share) and emits `output.message.started`. Doesn't touch `phase` or
    /// `iterations`, so it's safe to call again after a crash without
    /// double-counting (a retry simply mints a fresh correlation id for the
    /// re-attempted stream).
    pub fn on_reason_started(&mut self, model: Option<&str>) -> Vec<EventData> {
        let message_id = MessageId::new();
        self.current_message_id = Some(message_id);
        vec![EventData::OutputMessageStarted {
            message_id,
            model: model.map(str::to_string),
            iteration: Some(self.iterations as u32 + 1),
        }]
    }

    /// Apply an LLM response. Emits `output.message.completed`, then either
    /// finishes the turn (`turn.completed` / `turn.failed` on
    /// max-iterations) or moves to `PendingAct` with the requested tool
    /// calls.
    pub fn on_reason_completed(&mut self, response: &ChatResponse) -> Vec<EventData> {
        self.iterations += 1;
        self.usage.add(response.usage);
        // Same id as the started event (fresh one if started wasn't run, e.g.
        // a non-streaming host driving the machine directly).
        let message_id = self.current_message_id.take().unwrap_or_default();
        let mut effects = vec![EventData::OutputMessageCompleted {
            message_id,
            message: response.message.clone(),
            usage: response.usage,
        }];

        if response.message.tool_calls.is_empty() {
            let outcome = TurnOutcome::Success {
                response: response.message.text(),
            };
            effects.push(EventData::TurnCompleted {
                iterations: self.iterations,
                tool_calls: self.tool_calls_executed,
            });
            self.phase = TurnPhase::Completed(outcome);
        } else if self.iterations >= self.max_iterations {
            // Sealing rather than executing tools that can never be reasoned
            // over again.
            let outcome = TurnOutcome::MaxIterations;
            effects.push(EventData::TurnFailed {
                error: format!("turn exceeded max iterations ({})", self.max_iterations),
            });
            self.phase = TurnPhase::Completed(outcome);
        } else {
            let calls = response
                .message
                .tool_calls
                .iter()
                .map(|call| PendingCall {
                    call: call.clone(),
                    started: false,
                    output: None,
                })
                .collect();
            self.phase = TurnPhase::PendingAct { calls };
        }
        effects
    }

    /// Replace the call a middleware rewrote, so the batch — and therefore
    /// `tool.started`, the executed call, and a resumed host's
    /// [`Self::next_action`] — carries what will actually run rather than
    /// what the model originally asked for. Emits `tool.rewritten`.
    ///
    /// The rewrite lives in the state, not just in the event stream, because
    /// a durable host must not re-run the *original* call after a crash
    /// between the rewrite and the execution. Ignored once the call has
    /// started, which makes it idempotent across a retry the same way
    /// [`Self::on_tool_started`] is.
    pub fn on_tool_rewritten(
        &mut self,
        call_id: &str,
        rewritten: ToolCall,
        by: Option<String>,
    ) -> Vec<EventData> {
        let TurnPhase::PendingAct { calls } = &mut self.phase else {
            return Vec::new();
        };
        let Some(pending) = calls.iter_mut().find(|p| p.call.id == call_id) else {
            return Vec::new();
        };
        if pending.started || pending.output.is_some() {
            return Vec::new();
        }
        // The batch is keyed by id; a rewrite changes what runs, never which
        // call it is.
        pending.call = ToolCall {
            id: pending.call.id.clone(),
            ..rewritten
        };
        vec![EventData::ToolRewritten {
            call_id: pending.call.id.clone(),
            name: pending.call.name.clone(),
            call: Some(pending.call.clone()),
            by,
        }]
    }

    /// Record that the call with this id is starting. Idempotent per call
    /// id: re-running after a crash records nothing twice.
    pub fn on_tool_started(&mut self, call_id: &str) -> Vec<EventData> {
        let TurnPhase::PendingAct { calls } = &mut self.phase else {
            return Vec::new();
        };
        let Some(pending) = calls.iter_mut().find(|p| p.call.id == call_id) else {
            return Vec::new();
        };
        if pending.started {
            return Vec::new();
        }
        pending.started = true;
        vec![EventData::ToolStarted {
            call: pending.call.clone(),
        }]
    }

    /// Apply a call's result by id. Emits `tool.completed`; when every call
    /// in the batch has resolved, moves back to `PendingReason` — the batch
    /// can complete in any order.
    pub fn on_tool_completed(&mut self, call_id: &str, output: &ToolOutput) -> Vec<EventData> {
        let TurnPhase::PendingAct { calls } = &mut self.phase else {
            return Vec::new();
        };
        let Some(pending) = calls.iter_mut().find(|p| p.call.id == call_id) else {
            return Vec::new();
        };
        if pending.output.is_some() {
            return Vec::new();
        }
        pending.output = Some(output.clone());
        self.tool_calls_executed += 1;
        let effects = vec![EventData::ToolCompleted {
            call_id: pending.call.id.clone(),
            name: pending.call.name.clone(),
            output: output.content.clone(),
            is_error: output.is_error,
            metadata: output.metadata.clone(),
        }];
        if calls.iter().all(|p| p.output.is_some()) {
            self.phase = TurnPhase::PendingReason;
        }
        effects
    }

    /// Fail the turn (driver error, host abort). Emits `turn.failed`.
    pub fn on_failure(&mut self, error: impl Into<String>) -> Vec<EventData> {
        let error = error.into();
        self.phase = TurnPhase::Completed(TurnOutcome::Failed {
            error: error.clone(),
        });
        vec![EventData::TurnFailed { error }]
    }

    /// Stop the turn cooperatively (see
    /// [`crate::cancellation::CancellationToken`]), abandoning whatever is
    /// pending — including the rest of a not-yet-drained tool-call batch.
    /// Emits `turn.cancelled`.
    pub fn on_cancel(&mut self) -> Vec<EventData> {
        self.phase = TurnPhase::Completed(TurnOutcome::Cancelled);
        vec![EventData::TurnCancelled]
    }

    /// Stop the turn deliberately (see [`SealReason`]), abandoning whatever
    /// is pending. Emits `turn.sealed`.
    pub fn on_seal(&mut self, reason: SealReason) -> Vec<EventData> {
        self.phase = TurnPhase::Completed(TurnOutcome::Sealed(reason));
        vec![EventData::TurnSealed { reason }]
    }

    /// Whether the turn has finished, however it ended.
    pub fn is_complete(&self) -> bool {
        matches!(self.phase, TurnPhase::Completed(_))
    }

    /// How the turn ended, or `None` while it is still running.
    pub fn outcome(&self) -> Option<&TurnOutcome> {
        match &self.phase {
            TurnPhase::Completed(outcome) => Some(outcome),
            _ => None,
        }
    }
}

fn replay_state(state: &mut Option<TurnState>, turn_id: TurnId) -> Result<&mut TurnState> {
    state
        .as_mut()
        .ok_or_else(|| Error::EventLog(format!("turn {turn_id} event precedes turn.started")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventRequest;
    use crate::id::EventId;
    use crate::message::Message;
    use chrono::Utc;
    use serde_json::json;

    fn tool_call_response(names: &[&str]) -> ChatResponse {
        let calls = names
            .iter()
            .enumerate()
            .map(|(index, name)| ToolCall {
                id: format!("call_{index}"),
                name: (*name).to_string(),
                arguments: json!({}),
            })
            .collect();
        ChatResponse {
            message: Message::assistant_with_calls("", calls),
            usage: Usage::default(),
        }
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse {
            message: Message::assistant(text),
            usage: Usage::default(),
        }
    }

    #[test]
    fn happy_path_transitions() {
        let input = Message::user("hi");
        let (mut state, effects) = TurnState::start(SessionId::new(), 4, &input);
        assert_eq!(effects.len(), 2);
        assert_eq!(state.next_action(), TurnAction::Reason);

        state.on_reason_completed(&tool_call_response(&["a", "b"]));
        let TurnAction::ExecuteTool { call } = state.next_action() else {
            panic!("expected tool action");
        };
        assert_eq!(call.name, "a");

        assert_eq!(state.on_tool_started(&call.id).len(), 1);
        assert_eq!(state.on_tool_started(&call.id).len(), 0); // idempotent
        state.on_tool_completed(&call.id, &ToolOutput::text("ok"));

        let TurnAction::ExecuteTool { call } = state.next_action() else {
            panic!("expected second tool action");
        };
        assert_eq!(call.name, "b");
        state.on_tool_started(&call.id);
        state.on_tool_completed(&call.id, &ToolOutput::text("ok"));

        assert_eq!(state.next_action(), TurnAction::Reason);
        let effects = state.on_reason_completed(&text_response("done"));
        assert!(matches!(
            effects.last(),
            Some(EventData::TurnCompleted { .. })
        ));
        assert_eq!(
            state.outcome(),
            Some(&TurnOutcome::Success {
                response: "done".into()
            })
        );
        assert_eq!(state.iterations, 2);
        assert_eq!(state.tool_calls_executed, 2);
    }

    #[test]
    fn reason_started_and_completed_share_a_message_id() {
        let input = Message::user("hi");
        let (mut state, _) = TurnState::start(SessionId::new(), 4, &input);

        let started = state.on_reason_started(Some("m"));
        let EventData::OutputMessageStarted {
            message_id: start_id,
            ..
        } = started[0]
        else {
            panic!("expected output.message.started");
        };
        assert_eq!(state.current_message_id, Some(start_id));

        let completed = state.on_reason_completed(&text_response("done"));
        let EventData::OutputMessageCompleted {
            message_id: done_id,
            ..
        } = completed[0]
        else {
            panic!("expected output.message.completed");
        };
        assert_eq!(start_id, done_id, "started/completed must correlate");
        // Cleared once the message is complete.
        assert_eq!(state.current_message_id, None);
    }

    #[test]
    fn max_iterations_seals_instead_of_acting() {
        let input = Message::user("go");
        let (mut state, _) = TurnState::start(SessionId::new(), 1, &input);
        let effects = state.on_reason_completed(&tool_call_response(&["a"]));
        assert!(matches!(effects.last(), Some(EventData::TurnFailed { .. })));
        assert_eq!(state.outcome(), Some(&TurnOutcome::MaxIterations));
    }

    #[test]
    fn state_survives_serialization_mid_turn() {
        let input = Message::user("hi");
        let (mut state, _) = TurnState::start(SessionId::new(), 4, &input);
        state.on_reason_completed(&tool_call_response(&["a"]));
        let TurnAction::ExecuteTool { call } = state.next_action() else {
            panic!("expected tool action");
        };
        state.on_tool_started(&call.id);

        let json = serde_json::to_string(&state).unwrap();
        let mut restored: TurnState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);

        // The resumed host re-issues the same action, records nothing twice,
        // and continues.
        assert!(matches!(
            restored.next_action(),
            TurnAction::ExecuteTool { .. }
        ));
        assert_eq!(restored.on_tool_started(&call.id).len(), 0);
        restored.on_tool_completed(&call.id, &ToolOutput::text("ok"));
        assert_eq!(restored.next_action(), TurnAction::Reason);
    }

    #[test]
    fn pending_tool_actions_returns_the_whole_not_started_batch() {
        let input = Message::user("hi");
        let (mut state, _) = TurnState::start(SessionId::new(), 4, &input);
        state.on_reason_completed(&tool_call_response(&["a", "b", "c"]));

        let actions = state.pending_tool_actions();
        assert_eq!(actions.len(), 3);

        // Starting one call removes only that call from the ready batch.
        let TurnAction::ExecuteTool { call } = &actions[0] else {
            panic!("expected tool action");
        };
        state.on_tool_started(&call.id);
        assert_eq!(state.pending_tool_actions().len(), 2);
    }

    #[test]
    fn a_rewrite_changes_what_started_announces_and_what_a_resume_executes() {
        let input = Message::user("hi");
        let (mut state, _) = TurnState::start(SessionId::new(), 4, &input);
        state.on_reason_completed(&tool_call_response(&["save"]));
        let TurnAction::ExecuteTool { call } = state.next_action() else {
            panic!("expected tool action");
        };

        let redacted = ToolCall {
            id: "ignored — the batch is keyed by the original id".into(),
            name: call.name.clone(),
            arguments: json!({"secret": "[redacted]"}),
        };
        let effects = state.on_tool_rewritten(&call.id, redacted, Some("redactor".into()));
        assert!(matches!(
            effects.as_slice(),
            [EventData::ToolRewritten { by: Some(by), .. }] if by == "redactor"
        ));

        // `tool.started` announces the call as it will actually run.
        let started = state.on_tool_started(&call.id);
        let [EventData::ToolStarted { call: announced }] = started.as_slice() else {
            panic!("expected tool.started");
        };
        assert_eq!(announced.id, call.id, "a rewrite keeps the call id");
        assert_eq!(announced.arguments, json!({"secret": "[redacted]"}));

        // And a host resuming from this state runs the rewrite, not the
        // original — the reason the rewrite is state and not just an event.
        let restored: TurnState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let TurnAction::ExecuteTool { call: resumed } = restored.next_action() else {
            panic!("expected the rewritten call");
        };
        assert_eq!(resumed.arguments, json!({"secret": "[redacted]"}));

        // Once started, a re-run of the chain cannot rewrite again.
        assert!(state.on_tool_rewritten(&call.id, resumed, None).is_empty());
    }

    #[test]
    fn batch_completes_out_of_order() {
        let input = Message::user("hi");
        let (mut state, _) = TurnState::start(SessionId::new(), 4, &input);
        state.on_reason_completed(&tool_call_response(&["a", "b"]));

        let ids: Vec<String> = state
            .pending_tool_actions()
            .into_iter()
            .map(|action| {
                let TurnAction::ExecuteTool { call } = action else {
                    unreachable!()
                };
                call.id
            })
            .collect();
        for id in &ids {
            state.on_tool_started(id);
        }

        // Complete "b" (the second call) before "a" — still ends up back at
        // PendingReason with both counted, regardless of completion order.
        state.on_tool_completed(&ids[1], &ToolOutput::text("b done"));
        let TurnAction::ExecuteTool { call: remaining } = state.next_action() else {
            panic!("expected the still-incomplete call");
        };
        assert_eq!(remaining.id, ids[0]);
        state.on_tool_completed(&ids[0], &ToolOutput::text("a done"));

        assert_eq!(state.next_action(), TurnAction::Reason);
        assert_eq!(state.tool_calls_executed, 2);
    }

    #[test]
    fn replay_rejects_unknown_state_bearing_events() {
        let session_id = SessionId::new();
        let (state, effects) = TurnState::start(session_id, 4, &Message::user("go"));
        let turn_id = state.turn_id;
        let mut events: Vec<Event> = effects
            .into_iter()
            .enumerate()
            .map(|(index, data)| {
                EventRequest::with_turn(session_id, turn_id, data).into_event(
                    EventId::new(),
                    Utc::now(),
                    index as u64 + 1,
                )
            })
            .collect();
        events.push(
            EventRequest::with_turn(
                session_id,
                turn_id,
                EventData::Custom {
                    event_type: "turn.future_transition".into(),
                    payload: serde_json::json!({}),
                    state_bearing: true,
                },
            )
            .into_event(EventId::new(), Utc::now(), 3),
        );

        let error = TurnState::replay(&events, turn_id).unwrap_err();
        assert!(error.to_string().contains("unknown state-bearing event"));
    }
}
