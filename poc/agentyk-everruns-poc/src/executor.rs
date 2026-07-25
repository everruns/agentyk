//! A satellite [`TurnExecutor`] proving everruns-style act-loop behavior lives
//! in the executor layer, not in agentyk-core's hook trait. Built entirely
//! over core's public seams (`atoms` + [`TurnState`] + [`TurnHost`]).
//!
//! It shows the three shapes of adoption "gap 4" that core's
//! `PreToolUseHook`/`PreToolUseDecision` can't express, all in a satellite:
//!
//! - **Deny with a user-facing message** — [`ApprovalDecision::Deny`] /
//!   [`GuardOutcome::Deny`] carry a `user_message`.
//! - **Mutate a call before it runs** — [`GuardOutcome::Rewrite`] (e.g. a
//!   guard that redacts a secret argument).
//! - **Capability-contributed guards** — a satellite capability bundles a tool
//!   *and* the [`PreToolGuard`] that governs it; the host attaches the tool as
//!   a `Capability` and the guard to the executor.
//!
//! It also **dispatches a tool batch concurrently** (via
//! [`TurnState::pending_tool_actions`](agentyk_core::turn::TurnState::pending_tool_actions)),
//! which the built-in `InProcessExecutor` deliberately doesn't. Otherwise
//! minimal (non-streaming, no budget seam).

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
/// for the blocked ones and `tool.rewritten` for the redacted ones.
enum Resolved {
    Ran { output: ToolOutput, rewritten: bool },
    Denied(String),
}

/// What a [`PreToolGuard`] decides for a call. Richer than core's
/// `PreToolUseDecision` (`Allow | Deny`): a guard can also **rewrite** the
/// call — the everruns redaction/mutation shape — and a denial carries a
/// user-facing message.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardOutcome {
    Allow,
    /// Proceed, but with this (mutated) call — e.g. a redacted argument. The
    /// call keeps its id (the batch is keyed by it); rewrite name/arguments.
    Rewrite(ToolCall),
    Deny {
        user_message: String,
    },
}

/// Runs before each tool call in [`EverrunsExecutor`]'s act loop; guards run
/// in order, a `Rewrite` feeds the next guard and the execution, and the first
/// `Deny` short-circuits. This is where a satellite puts mutating / approval /
/// capability-contributed guardrails.
#[async_trait]
pub trait PreToolGuard: Send + Sync {
    async fn inspect(
        &self,
        call: &ToolCall,
        hints: &ToolHints,
        context: &ToolContext,
    ) -> GuardOutcome;
}

/// What an [`Approver`] decides for a risky tool call — the simple
/// allow/deny-with-message case, wrapped into a [`PreToolGuard`] by
/// [`EverrunsExecutor::new`].
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

/// Adapts an [`Approver`] into a [`PreToolGuard`] that only consults it for
/// tools whose hints say they [`need approval`](ToolHints::needs_approval).
struct ApprovalGuard(Arc<dyn Approver>);

#[async_trait]
impl PreToolGuard for ApprovalGuard {
    async fn inspect(
        &self,
        call: &ToolCall,
        hints: &ToolHints,
        _context: &ToolContext,
    ) -> GuardOutcome {
        if !hints.needs_approval() {
            return GuardOutcome::Allow;
        }
        match self.0.approve(call, hints).await {
            ApprovalDecision::Allow => GuardOutcome::Allow,
            ApprovalDecision::Deny { user_message } => GuardOutcome::Deny { user_message },
        }
    }
}

/// A [`TurnExecutor`] that runs a chain of [`PreToolGuard`]s before each tool
/// call, reading each tool's risk from the `ToolDefinition.metadata` hints
/// hatch. Everything else follows the standard reason/act loop.
pub struct EverrunsExecutor {
    guards: Vec<Arc<dyn PreToolGuard>>,
}

impl EverrunsExecutor {
    /// An executor whose only guard is hint-based approval via `approver`.
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self {
            guards: vec![Arc::new(ApprovalGuard(Arc::new(approver)))],
        }
    }

    /// An executor with an explicit guard chain (approval, redaction,
    /// capability-contributed guards — in order).
    pub fn with_guards(guards: Vec<Arc<dyn PreToolGuard>>) -> Self {
        Self { guards }
    }

    /// Append a guard (runs after existing ones).
    pub fn guard(mut self, guard: impl PreToolGuard + 'static) -> Self {
        self.guards.push(Arc::new(guard));
        self
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
        let assembled = atoms::assemble(
            host.config.system_prompt.as_str(),
            &host.config.capabilities,
            host.session_id,
        )
        .await?;
        let driver = host
            .config
            .drivers
            .get(&host.model.driver)
            .ok_or_else(|| Error::UnknownDriver(host.model.driver.to_string()))?;
        let model = host.model.clone();

        let (mut state, effects) =
            TurnState::start(host.session_id, host.config.max_iterations, &input);
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
                        .config
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
                    // call at a time.
                    let calls: Vec<ToolCall> = state
                        .pending_tool_actions()
                        .into_iter()
                        .filter_map(|action| match action {
                            TurnAction::ExecuteTool { call } => Some(call),
                            _ => None,
                        })
                        .collect();

                    // Record all starts sequentially — `record` needs `&mut host`.
                    // A tool's risk hint (from the `metadata` hatch) rides the
                    // event stream as a `tool.hint` custom event — everruns'
                    // richer observability with no core variant, so a pure
                    // observer like `NarrationListener` can surface it.
                    for call in &calls {
                        if let Some(label) = assembled
                            .tool(&call.name)
                            .map(|tool| tool.definition())
                            .as_ref()
                            .and_then(ToolHints::from_definition)
                            .and_then(|hints| hints.label())
                        {
                            host.record(
                                turn_id,
                                vec![EventData::custom(
                                    "tool.hint",
                                    serde_json::json!({ "name": call.name, "risk": label }),
                                )],
                            )
                            .await?;
                        }
                        let effects = state.on_tool_started(&call.id);
                        host.record(turn_id, effects).await?;
                    }

                    let context = ToolContext::new(host.session_id, turn_id)
                        .with_extensions(host.config.extensions.clone());

                    // Run the batch concurrently. Each future runs the guard
                    // chain for its call (rewrites feed the next guard and the
                    // execution; the first deny short-circuits), then acts —
                    // no `&mut host` inside, so they compose under `join_all`.
                    let assembled = &assembled;
                    let context = &context;
                    let guards = &self.guards;
                    let resolved = join_all(calls.iter().map(|call| {
                        let hints = assembled
                            .tool(&call.name)
                            .map(|tool| tool.definition())
                            .as_ref()
                            .and_then(ToolHints::from_definition)
                            .unwrap_or_default();
                        async move {
                            let mut effective = call.clone();
                            let mut rewritten = false;
                            for guard in guards {
                                match guard.inspect(&effective, &hints, context).await {
                                    GuardOutcome::Allow => {}
                                    GuardOutcome::Rewrite(next) => {
                                        effective = next;
                                        rewritten = true;
                                    }
                                    GuardOutcome::Deny { user_message } => {
                                        return Resolved::Denied(user_message);
                                    }
                                }
                            }
                            Resolved::Ran {
                                output: atoms::act(assembled, &effective, context).await,
                                rewritten,
                            }
                        }
                    }))
                    .await;

                    // Record completions (and any denials) sequentially, in
                    // batch order — by each call's original id.
                    for (call, outcome) in calls.iter().zip(resolved) {
                        let output = match outcome {
                            Resolved::Ran { output, rewritten } => {
                                // A guard mutated the call before it ran (e.g.
                                // redaction) — announce it on the event stream
                                // so observers can show the redaction happened.
                                if rewritten {
                                    host.record(
                                        turn_id,
                                        vec![EventData::custom(
                                            "tool.rewritten",
                                            serde_json::json!({ "name": call.name }),
                                        )],
                                    )
                                    .await?;
                                }
                                output
                            }
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
                        TurnOutcome::Success { response } => Ok(TurnResult::new(
                            turn_id,
                            response,
                            state.iterations,
                            state.tool_calls_executed,
                            state.usage,
                        )),
                        TurnOutcome::MaxIterations => {
                            Err(Error::MaxIterations(host.config.max_iterations))
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
