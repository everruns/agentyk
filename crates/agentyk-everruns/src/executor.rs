//! A satellite [`TurnExecutor`] that adds everruns-style **hint-based tool
//! approval** — behavior agentyk-core's built-in executor doesn't have, built
//! entirely over core's public seams (`atoms` + [`TurnState`] + [`TurnHost`]).
//!
//! This is the proof for `docs/extensibility.md`: gap 4 (mutating / approval /
//! capability-contributed guardrails) lives in the executor layer, not in
//! core's hook trait. Note in particular [`ApprovalDecision::Deny`] carries a
//! `user_message` — a richer decision than core's `PreToolUseDecision`, owned
//! here rather than forced into the contract.
//!
//! It also **dispatches a tool batch concurrently** (via
//! [`TurnState::pending_tool_actions`](agentyk_core::turn::TurnState::pending_tool_actions)),
//! which the built-in `InProcessExecutor` deliberately doesn't — closing
//! agentyk's item-9 "concurrent dispatch is a deferred follow-up" note in a
//! satellite. It stays otherwise minimal (non-streaming, no budget seam).

use agentyk_core::atoms;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::message::{Message, ToolCall};
use agentyk_core::tool::{ToolContext, ToolOutput};
use agentyk_core::turn::{TurnAction, TurnOutcome, TurnState};
use async_trait::async_trait;
use futures_util::future::join_all;
use std::sync::Arc;

use crate::hints::ToolHints;

/// How one tool call in a batch resolved during the concurrent act phase —
/// carried back so the (sequential) recording phase can emit `tool.denied`
/// for the blocked ones.
enum Resolved {
    Ran(ToolOutput),
    Denied(String),
}

/// What an [`Approver`] decides for a risky tool call. `Deny` carries a
/// `user_message` distinct from any internal reason — the shape everruns
/// guardrails want and core's `PreToolUseDecision` lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny { user_message: String },
}

/// Consulted before a tool whose [`ToolHints`] say it
/// [`needs_approval`](ToolHints::needs_approval) runs. A host implements this
/// to pause for a human, apply a policy, etc.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, call: &ToolCall, hints: &ToolHints) -> ApprovalDecision;
}

/// An approver that permits everything — the no-op baseline.
pub struct AllowAll;

#[async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _call: &ToolCall, _hints: &ToolHints) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// A [`TurnExecutor`] that gates destructive/open-world tools through an
/// [`Approver`], reading each tool's risk from the `ToolDefinition.metadata`
/// hints hatch. Everything else follows the standard reason/act loop.
pub struct EverrunsExecutor {
    approver: Arc<dyn Approver>,
}

impl EverrunsExecutor {
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self {
            approver: Arc::new(approver),
        }
    }
}

impl Default for EverrunsExecutor {
    fn default() -> Self {
        Self::new(AllowAll)
    }
}

#[async_trait]
impl TurnExecutor for EverrunsExecutor {
    async fn run_turn(&self, host: &mut TurnHost<'_>, input: Message) -> Result<TurnResult> {
        let assembled =
            atoms::assemble(host.system_prompt, host.capabilities, host.session_id).await?;
        let driver = host
            .drivers
            .get(&host.model.driver)
            .ok_or_else(|| Error::UnknownDriver(host.model.driver.to_string()))?;
        let model = host.model.clone();

        let (mut state, effects) = TurnState::start(host.session_id, host.max_iterations, &input);
        let turn_id = state.turn_id;
        host.record(turn_id, effects).await?;

        loop {
            if host.cancellation.is_cancelled() {
                let effects = state.on_cancel();
                host.record(turn_id, effects).await?;
                return Err(Error::Cancelled);
            }

            match state.next_action() {
                TurnAction::Reason => {
                    let started = state.on_reason_started(Some(&model.model));
                    host.record(turn_id, started).await?;
                    let messages = host
                        .context_assembler
                        .assemble(host.session_id, host.messages)
                        .await;
                    match atoms::reason(driver.as_ref(), &model, &assembled, messages).await {
                        Ok(response) => {
                            let effects = state.on_reason_completed(&response);
                            host.record(turn_id, effects).await?;
                        }
                        Err(error) => {
                            let effects = state.on_failure(error.to_string());
                            host.record(turn_id, effects).await?;
                            return Err(error);
                        }
                    }
                }
                TurnAction::ExecuteTool { .. } => {
                    // Parallel dispatch: fan out the whole not-yet-started
                    // batch at once (agentyk's item-9 follow-up) instead of one
                    // call at a time. The data model already supports this via
                    // `pending_tool_actions`; this is a satellite executor
                    // proving it end-to-end.
                    let calls: Vec<ToolCall> = state
                        .pending_tool_actions()
                        .into_iter()
                        .filter_map(|action| match action {
                            TurnAction::ExecuteTool { call } => Some(call),
                            _ => None,
                        })
                        .collect();

                    // Record all starts sequentially — `record` needs `&mut host`.
                    for call in &calls {
                        let effects = state.on_tool_started(&call.id);
                        host.record(turn_id, effects).await?;
                    }

                    let context = ToolContext {
                        session_id: host.session_id,
                        turn_id,
                        extensions: host.extensions.clone(),
                    };

                    // Run the batch concurrently. Each future is self-contained
                    // (reads hints, consults the approver, then acts or resolves
                    // to a denial) — no `&mut host` inside, so they compose
                    // under `join_all`.
                    let assembled = &assembled;
                    let context = &context;
                    let resolved = join_all(calls.iter().map(|call| {
                        let approver = self.approver.clone();
                        let hints = assembled
                            .tool(&call.name)
                            .map(|tool| tool.definition())
                            .as_ref()
                            .and_then(ToolHints::from_definition)
                            .unwrap_or_default();
                        async move {
                            if hints.needs_approval() {
                                match approver.approve(call, &hints).await {
                                    ApprovalDecision::Allow => {
                                        Resolved::Ran(atoms::act(assembled, call, context).await)
                                    }
                                    ApprovalDecision::Deny { user_message } => {
                                        Resolved::Denied(user_message)
                                    }
                                }
                            } else {
                                Resolved::Ran(atoms::act(assembled, call, context).await)
                            }
                        }
                    }))
                    .await;

                    // Record completions (and any denials) sequentially, in
                    // batch order.
                    for (call, outcome) in calls.iter().zip(resolved) {
                        let output = match outcome {
                            Resolved::Ran(output) => output,
                            Resolved::Denied(user_message) => {
                                host.record(
                                    turn_id,
                                    vec![EventData::ToolDenied {
                                        call_id: call.id.clone(),
                                        name: call.name.clone(),
                                        reason: user_message.clone(),
                                    }],
                                )
                                .await?;
                                ToolOutput::error(user_message)
                            }
                        };
                        let effects = state.on_tool_completed(&call.id, &output);
                        host.record(turn_id, effects).await?;
                    }
                }
                TurnAction::Complete(outcome) => {
                    return match outcome {
                        TurnOutcome::Success { response } => Ok(TurnResult {
                            turn_id,
                            response,
                            iterations: state.iterations,
                            tool_calls: state.tool_calls_executed,
                            usage: state.usage,
                        }),
                        TurnOutcome::MaxIterations => {
                            Err(Error::MaxIterations(host.max_iterations))
                        }
                        TurnOutcome::Failed { error } => Err(Error::Other(error)),
                        TurnOutcome::Cancelled => Err(Error::Cancelled),
                        TurnOutcome::Sealed(reason) => Err(Error::Sealed(reason)),
                    };
                }
            }
        }
    }
}
