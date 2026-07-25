//! A satellite [`TurnExecutor`] proving everruns-style act-loop behavior lives
//! outside agentyk-core. Built entirely over core's public seams (`atoms` +
//! [`TurnState`] + [`TurnHost`] + [`TurnMiddleware`]).
//!
//! The division it demonstrates:
//!
//! - **Policy is middleware, not an executor.** [`ApprovalMiddleware`] denies
//!   a risky call with a user-facing message, reading the tool's risk from the
//!   `metadata` hatch that core hands to middleware without knowing its
//!   schema. Compose it with a redaction middleware that rewrites a call, and
//!   with a capability that contributes the middleware governing its own tool
//!   — the three shapes of adoption gap 4, none of them requiring this type.
//! - **Strategy is the executor.** [`EverrunsExecutor`] exists for one reason:
//!   it **dispatches a tool batch concurrently** (via
//!   [`TurnState::pending_tool_actions`](agentyk_core::turn::TurnState::pending_tool_actions)),
//!   which the built-in `InProcessExecutor` deliberately does not. It applies
//!   the same core middleware chain the built-in executor does, so the two
//!   cannot disagree about what middleware means. Otherwise minimal
//!   (non-streaming, no budget seam).

use agentyk_core::atoms;
use agentyk_core::error::{Error, Result};
use agentyk_core::event::EventData;
use agentyk_core::executor::{TurnExecutor, TurnHost, TurnResult};
use agentyk_core::message::{Message, ToolCall};
use agentyk_core::middleware::{
    ToolCallDecision, ToolChainOutcome, ToolInvocation, TurnMiddleware, after_tool_chain,
    before_tool_chain,
};
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
    Ran {
        call: ToolCall,
        output: ToolOutput,
        rewritten: bool,
    },
    Denied(String),
}

/// What an [`Approver`] decides for a risky tool call — the simple
/// allow/deny-with-message case, wrapped into middleware by
/// [`ApprovalMiddleware`].
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

/// Adapts an [`Approver`] into core [`TurnMiddleware`], consulting it only for
/// tools whose hints say they [`need approval`](ToolHints::needs_approval).
///
/// This used to be a satellite-only `PreToolGuard` trait plus a forked act
/// loop to run it, because core's hooks could only allow or deny. Core
/// middleware covers deny *and* rewrite, so the whole parallel trait
/// hierarchy is gone — what is left here is a policy, attached with
/// `AgentBuilder::middleware`.
pub struct ApprovalMiddleware(Arc<dyn Approver>);

impl ApprovalMiddleware {
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self(Arc::new(approver))
    }
}

#[async_trait]
impl TurnMiddleware for ApprovalMiddleware {
    fn name(&self) -> &str {
        "everruns-approval"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        // Hints ride `ToolDefinition.metadata`, which core hands to
        // middleware without knowing the schema — the metadata hatch and the
        // middleware seam meeting exactly where they should.
        let hints = invocation
            .definition
            .and_then(ToolHints::from_definition)
            .unwrap_or_default();
        if !hints.needs_approval() {
            return ToolCallDecision::Proceed;
        }
        match self.0.approve(invocation.call, &hints).await {
            ApprovalDecision::Allow => ToolCallDecision::Proceed,
            ApprovalDecision::Deny { user_message } => ToolCallDecision::Deny {
                reason: user_message,
            },
        }
    }
}

/// A [`TurnExecutor`] that dispatches a tool batch **concurrently**, which the
/// built-in `InProcessExecutor` deliberately does not, and narrates tool risk
/// hints onto the event stream as custom events.
///
/// Guard policy is no longer part of it: denial and rewriting are core
/// [`TurnMiddleware`], read from the agent's config and applied through
/// [`before_tool_chain`], so this executor and the built-in one cannot drift
/// on what a chain of middleware means. Dispatch strategy is the only reason
/// this type still exists — which is what an executor should be for.
#[derive(Debug, Default, Clone, Copy)]
pub struct EverrunsExecutor;

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
                        .assemble(host.session_id, host.history.messages())
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
                    }

                    let context = ToolContext::new(host.session_id, turn_id)
                        .with_extensions(host.config.extensions.clone());

                    // Run the batch concurrently. Each future resolves its own
                    // middleware chain via core's `before_tool_chain`, then
                    // acts — no `&mut host` inside, so they compose under
                    // `join_all`. The chain semantics (rewrite feeds the next,
                    // first deny wins) are core's, not this executor's.
                    let assembled = &assembled;
                    let context = &context;
                    let middleware = &host.config.middleware;
                    let resolved = join_all(calls.iter().map(|call| {
                        let definition = assembled.tool(&call.name).map(|tool| tool.definition());
                        async move {
                            match before_tool_chain(middleware, call, definition.as_ref(), context)
                                .await
                            {
                                ToolChainOutcome::Deny { reason } => Resolved::Denied(reason),
                                ToolChainOutcome::Proceed {
                                    call: executed,
                                    rewritten,
                                } => {
                                    let output = atoms::act(assembled, &executed, context).await;
                                    let output = after_tool_chain(
                                        middleware,
                                        &executed,
                                        definition.as_ref(),
                                        context,
                                        output,
                                    )
                                    .await;
                                    Resolved::Ran {
                                        call: executed,
                                        output,
                                        rewritten,
                                    }
                                }
                            }
                        }
                    }))
                    .await;

                    // Record starts and completions sequentially, in batch
                    // order — `record` needs `&mut host`.
                    for (call, outcome) in calls.iter().zip(resolved) {
                        let output = match outcome {
                            Resolved::Ran {
                                call: executed,
                                output,
                                rewritten,
                            } => {
                                if rewritten {
                                    let effects = state.on_tool_rewritten(
                                        &call.id,
                                        executed,
                                        Some("everruns-guard".into()),
                                    );
                                    host.record(turn_id, effects).await?;
                                }
                                let effects = state.on_tool_started(&call.id);
                                host.record(turn_id, effects).await?;
                                output
                            }
                            Resolved::Denied(user_message) => {
                                let effects = state.on_tool_started(&call.id);
                                host.record(turn_id, effects).await?;
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
