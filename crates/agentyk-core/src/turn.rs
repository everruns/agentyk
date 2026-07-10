//! Turn state machine — the execution abstraction.
//!
//! Adopted from everruns-core's `TurnStateMachine` / `RuntimeTurnState`, with
//! one sharpening: the machine here is **sans-IO**. Transitions are pure —
//! they mutate bookkeeping and return the [`EventData`] to record — and the
//! only side-effecting operations are the two atoms ([`crate::atoms::reason`]
//! and [`crate::atoms::act`]) that a host runs between transitions.
//!
//! ```text
//! start ──▶ PendingReason ──reason──▶ PendingAct ──act──▶ PendingReason ──▶ …
//!                │                                                │
//!                └────────────▶ Completed(TurnOutcome) ◀──────────┘
//! ```
//!
//! The driving contract, shared by every host:
//!
//! ```text
//! let (mut state, effects) = TurnState::start(session_id, max_iterations, &input);
//! record(effects);
//! loop {
//!     match state.next_action() {                    // pure peek
//!         TurnAction::Reason => {
//!             let response = atoms::reason(…).await;  // effectful atom
//!             record(state.on_reason_completed(&response));
//!         }
//!         TurnAction::ExecuteTool { call } => {
//!             record(state.on_tool_started());        // idempotent
//!             let output = atoms::act(…, &call, …).await;
//!             record(state.on_tool_completed(&output));
//!         }
//!         TurnAction::Complete(outcome) => break,
//!     }
//! }
//! ```
//!
//! ## Why this supports durable execution
//!
//! - [`TurnState`] is `Serialize`/`Deserialize` and carries **bookkeeping
//!   only** — no credentials, no tool objects, no message history (mirroring
//!   everruns' `RuntimeTurnState`). A durable host checkpoints it between
//!   activities and rebuilds the environment (assembled tools, history) from
//!   the agent value and the event log on each step.
//! - Each [`TurnAction`] is one activity. Re-issuing the current action after
//!   a crash is safe: `next_action` is a pure read, and `on_tool_started` is
//!   idempotent.
//! - Effects are data, so a host can persist the new state and append its
//!   events in one transaction — event emission never races execution.
//! - Message history is a fold over the event log
//!   ([`crate::session::messages_from_events`]), so replay is sufficient to
//!   resume mid-turn.

use serde::{Deserialize, Serialize};

use crate::driver::{ChatResponse, Usage};
use crate::event::EventData;
use crate::id::{SessionId, TurnId};
use crate::message::{Message, ToolCall};
use crate::tool::ToolOutput;

/// How a turn ended. Mirrors everruns' `TurnOutcome`
/// (Success/Failed/MaxIterations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcome {
    Success { response: String },
    Failed { error: String },
    MaxIterations,
}

/// Current phase of the turn. `PendingAct` carries the not-yet-executed tool
/// calls so a resumed host knows exactly what remains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TurnPhase {
    PendingReason,
    PendingAct {
        /// Tool calls awaiting execution, front first.
        pending: Vec<ToolCall>,
        /// Whether `tool.started` was already recorded for the front call —
        /// makes `on_tool_started` idempotent across crash/retry.
        started: bool,
    },
    Completed(TurnOutcome),
}

/// The next thing a host must do. One `TurnAction` maps to one durable
/// activity.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnAction {
    /// Run the LLM (the reason atom) over the current history.
    Reason,
    /// Execute one tool call (the act atom).
    ExecuteTool { call: ToolCall },
    /// The turn is finished.
    Complete(TurnOutcome),
}

/// Serializable turn bookkeeping. Everything else a step needs — model,
/// credentials, assembled tools, message history — is environment the host
/// provides per step; it is deliberately NOT part of this state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnState {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub max_iterations: usize,
    pub phase: TurnPhase,
    /// Completed reason (LLM) calls.
    pub iterations: usize,
    /// Executed tool calls.
    pub tool_calls_executed: usize,
    pub usage: Usage,
}

impl TurnState {
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
        };
        let effects = vec![
            EventData::TurnStarted,
            EventData::InputMessage {
                message: input.clone(),
            },
        ];
        (state, effects)
    }

    /// Pure peek at the next action. Idempotent — safe to call again after a
    /// crash without re-recording anything.
    pub fn next_action(&self) -> TurnAction {
        match &self.phase {
            TurnPhase::PendingReason => TurnAction::Reason,
            TurnPhase::PendingAct { pending, .. } => match pending.first() {
                Some(call) => TurnAction::ExecuteTool { call: call.clone() },
                // Unreachable by construction; degrade to another reason step.
                None => TurnAction::Reason,
            },
            TurnPhase::Completed(outcome) => TurnAction::Complete(outcome.clone()),
        }
    }

    /// Apply an LLM response. Emits `output.message`, then either finishes
    /// the turn (`turn.completed` / `turn.failed` on max-iterations) or moves
    /// to `PendingAct` with the requested tool calls.
    pub fn on_reason_completed(&mut self, response: &ChatResponse) -> Vec<EventData> {
        self.iterations += 1;
        self.usage.add(response.usage);
        let mut effects = vec![EventData::OutputMessage {
            message: response.message.clone(),
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
            self.phase = TurnPhase::PendingAct {
                pending: response.message.tool_calls.clone(),
                started: false,
            };
        }
        effects
    }

    /// Record that the front tool call is starting. Idempotent: re-running
    /// after a crash records nothing twice.
    pub fn on_tool_started(&mut self) -> Vec<EventData> {
        if let TurnPhase::PendingAct { pending, started } = &mut self.phase
            && !*started
            && let Some(call) = pending.first()
        {
            let effect = EventData::ToolStarted { call: call.clone() };
            *started = true;
            return vec![effect];
        }
        Vec::new()
    }

    /// Apply the front tool call's result. Emits `tool.completed`; when the
    /// batch is drained, moves back to `PendingReason`.
    pub fn on_tool_completed(&mut self, output: &ToolOutput) -> Vec<EventData> {
        let TurnPhase::PendingAct { pending, started } = &mut self.phase else {
            return Vec::new();
        };
        let Some(call) = pending.first().cloned() else {
            return Vec::new();
        };
        pending.remove(0);
        *started = false;
        self.tool_calls_executed += 1;
        let effects = vec![EventData::ToolCompleted {
            call_id: call.id,
            name: call.name,
            output: output.content.clone(),
            is_error: output.is_error,
        }];
        if pending.is_empty() {
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

    pub fn is_complete(&self) -> bool {
        matches!(self.phase, TurnPhase::Completed(_))
    }

    pub fn outcome(&self) -> Option<&TurnOutcome> {
        match &self.phase {
            TurnPhase::Completed(outcome) => Some(outcome),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
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

        assert_eq!(state.on_tool_started().len(), 1);
        assert_eq!(state.on_tool_started().len(), 0); // idempotent
        state.on_tool_completed(&ToolOutput::text("ok"));

        let TurnAction::ExecuteTool { call } = state.next_action() else {
            panic!("expected second tool action");
        };
        assert_eq!(call.name, "b");
        state.on_tool_started();
        state.on_tool_completed(&ToolOutput::text("ok"));

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
        state.on_tool_started();

        let json = serde_json::to_string(&state).unwrap();
        let mut restored: TurnState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);

        // The resumed host re-issues the same action, records nothing twice,
        // and continues.
        assert!(matches!(
            restored.next_action(),
            TurnAction::ExecuteTool { .. }
        ));
        assert_eq!(restored.on_tool_started().len(), 0);
        restored.on_tool_completed(&ToolOutput::text("ok"));
        assert_eq!(restored.next_action(), TurnAction::Reason);
    }
}
