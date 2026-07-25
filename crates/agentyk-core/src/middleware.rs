//! Middleware — interception points inside the turn loop.
//!
//! One trait with defaulted methods, rather than one trait per interception
//! point. That matters because interception points multiply: everruns alone
//! wants approval pauses, redaction, capability-contributed guards,
//! post-act transforms, and client-side tool handoff. As separate traits,
//! each of those is a new trait, a new `AgentDefinition` field, a new builder
//! setter, and new engine code. As methods on [`TurnMiddleware`], a new point
//! is a defaulted method and existing implementations keep compiling.
//!
//! Middleware replaces the earlier `PreToolUseHook` / `PostToolExecHook`
//! pair, and can do something they could not: **rewrite** a call before it
//! runs (argument redaction, path rewriting), which previously forced a
//! adopter to fork the whole turn loop.
//!
//! Ordering is the attachment order. [`before_tool_chain`] defines the
//! semantics the canonical engine applies — a rewrite feeds the next
//! middleware and the execution, and the first deny short-circuits.

use std::sync::Arc;

use async_trait::async_trait;

use crate::message::ToolCall;
use crate::tool::{ToolContext, ToolDefinition, ToolOutput};

/// One tool call about to run (or just finished), with everything a
/// middleware needs to judge it.
#[non_exhaustive]
pub struct ToolInvocation<'a> {
    /// The call as it currently stands — already carrying any rewrite an
    /// earlier middleware made.
    pub call: &'a ToolCall,
    /// The tool's definition, when the tool is known. Carries
    /// [`ToolDefinition::metadata`], which is where host-side taxonomies
    /// (risk hints, categories) live — so a middleware can gate on them
    /// without core knowing their schema. `None` for an unknown tool.
    pub definition: Option<&'a ToolDefinition>,
    /// Session, turn, and the host's injected services.
    pub context: &'a ToolContext,
}

impl<'a> ToolInvocation<'a> {
    /// Bundle what a middleware needs to judge one call.
    pub fn new(
        call: &'a ToolCall,
        definition: Option<&'a ToolDefinition>,
        context: &'a ToolContext,
    ) -> Self {
        Self {
            call,
            definition,
            context,
        }
    }
}

/// What a middleware decides about a tool call it is shown.
///
/// Deliberately exhaustive: an engine that silently ignored a new decision
/// would run a call a middleware meant to stop.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallDecision {
    /// No opinion — pass to the next middleware, then run the call.
    Proceed,
    /// Run this call instead. The original call's `id` is preserved for you
    /// — the turn's pending batch is keyed by it — so a rewrite may change
    /// the name and arguments but never the identity. Later middleware see
    /// the rewrite, and so do `tool.started`, the execution, and a resumed
    /// durable host.
    ///
    /// **Scope**: a rewrite governs the *act* phase. It cannot un-say what
    /// the model already generated — the assistant message recorded by
    /// `output.message.completed` still holds the call the model asked for.
    /// Redacting a secret this way keeps it out of the tool and out of every
    /// act-phase event, not out of the reason-phase record of the model's own
    /// output. Rewriting that would need a reason-phase interception point
    /// (everruns' `output.message.replaced`), which agentyk does not have.
    Rewrite(ToolCall),
    /// The call never runs. `reason` becomes the (error) result the model
    /// sees, and `tool.denied` is recorded.
    Deny {
        /// Why, in words the model can react to.
        reason: String,
    },
}

/// Intercepts points in a turn. Every method has a default, so an
/// implementation only writes the ones it cares about.
#[async_trait]
pub trait TurnMiddleware: Send + Sync {
    /// Human-readable name, for diagnostics and event payloads.
    fn name(&self) -> &str {
        "middleware"
    }

    /// Before a tool runs. See [`ToolCallDecision`]; the chain semantics are
    /// [`before_tool_chain`].
    async fn before_tool(&self, _invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        ToolCallDecision::Proceed
    }

    /// After a tool ran, in attachment order, each seeing the previous one's
    /// output. Runs for every executed call — including one the tool itself
    /// reported as an error — but never for a denied call.
    async fn after_tool(&self, _invocation: &ToolInvocation<'_>, output: ToolOutput) -> ToolOutput {
        output
    }
}

/// The resolution of a whole [`TurnMiddleware::before_tool`] chain.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChainOutcome {
    /// Run this call: the original, or the last rewrite of it.
    Proceed {
        /// What to execute.
        call: ToolCall,
        /// Whether any middleware rewrote the call, so the executor can
        /// record `tool.rewritten` — a redaction that left no trace would be
        /// a bad default.
        rewritten: bool,
    },
    /// Blocked before running.
    Deny {
        /// Why, from the middleware that stopped it.
        reason: String,
    },
}

/// Run the `before_tool` chain in order: a [`ToolCallDecision::Rewrite`]
/// feeds the next middleware (and ultimately the execution), and the first
/// [`ToolCallDecision::Deny`] short-circuits.
///
/// The canonical engine calls this rather than allowing hosts to reinterpret
/// middleware semantics.
pub async fn before_tool_chain(
    middleware: &[Arc<dyn TurnMiddleware>],
    call: &ToolCall,
    definition: Option<&ToolDefinition>,
    context: &ToolContext,
) -> ToolChainOutcome {
    let mut effective = call.clone();
    let mut rewritten = false;
    for entry in middleware {
        let invocation = ToolInvocation::new(&effective, definition, context);
        match entry.before_tool(&invocation).await {
            ToolCallDecision::Proceed => {}
            ToolCallDecision::Rewrite(mut next) => {
                // The batch is keyed by call id; a rewrite may change the
                // name and arguments but never the identity.
                next.id = effective.id.clone();
                effective = next;
                rewritten = true;
            }
            ToolCallDecision::Deny { reason } => return ToolChainOutcome::Deny { reason },
        }
    }
    ToolChainOutcome::Proceed {
        call: effective,
        rewritten,
    }
}

/// Run the `after_tool` chain in order, threading each middleware's output
/// into the next.
pub async fn after_tool_chain(
    middleware: &[Arc<dyn TurnMiddleware>],
    call: &ToolCall,
    definition: Option<&ToolDefinition>,
    context: &ToolContext,
    mut output: ToolOutput,
) -> ToolOutput {
    let invocation = ToolInvocation::new(call, definition, context);
    for entry in middleware {
        output = entry.after_tool(&invocation, output).await;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_0".into(),
            name: name.into(),
            arguments,
        }
    }

    /// Redacts one argument, the everruns mutation shape.
    struct Redact;

    #[async_trait]
    impl TurnMiddleware for Redact {
        async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
            let mut call = invocation.call.clone();
            if call.arguments.get("secret").is_some() {
                call.arguments["secret"] = json!("[redacted]");
                return ToolCallDecision::Rewrite(call);
            }
            ToolCallDecision::Proceed
        }
    }

    struct DenyNamed(&'static str);

    #[async_trait]
    impl TurnMiddleware for DenyNamed {
        async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
            if invocation.call.name == self.0 {
                return ToolCallDecision::Deny {
                    reason: format!("`{}` is not allowed", self.0),
                };
            }
            ToolCallDecision::Proceed
        }
    }

    /// Asserts it sees the *rewritten* call, then rewrites again.
    struct RequireRedacted;

    #[async_trait]
    impl TurnMiddleware for RequireRedacted {
        async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
            assert_eq!(invocation.call.arguments["secret"], json!("[redacted]"));
            let mut call = invocation.call.clone();
            call.arguments["seen_by"] = json!("second");
            ToolCallDecision::Rewrite(call)
        }
    }

    struct Suffix(&'static str);

    #[async_trait]
    impl TurnMiddleware for Suffix {
        async fn after_tool(
            &self,
            _invocation: &ToolInvocation<'_>,
            output: ToolOutput,
        ) -> ToolOutput {
            ToolOutput::new(format!("{}{}", output.content, self.0), output.is_error)
        }
    }

    #[test]
    fn a_rewrite_feeds_the_next_middleware_and_keeps_the_call_id() {
        let chain: Vec<Arc<dyn TurnMiddleware>> = vec![Arc::new(Redact), Arc::new(RequireRedacted)];
        let original = call("save", json!({"secret": "p@ss"}));
        let context = ToolContext::default();

        let outcome = block_on(before_tool_chain(&chain, &original, None, &context));
        let ToolChainOutcome::Proceed { call, rewritten } = outcome else {
            panic!("expected the chain to proceed");
        };
        assert!(rewritten);
        assert_eq!(call.id, original.id, "a rewrite must preserve the call id");
        assert_eq!(call.arguments["secret"], json!("[redacted]"));
        assert_eq!(call.arguments["seen_by"], json!("second"));
    }

    #[test]
    fn the_first_deny_short_circuits_the_rest_of_the_chain() {
        // If `Redact` ran after the deny it would panic on the missing
        // assertion in RequireRedacted; reaching it at all is the bug.
        let chain: Vec<Arc<dyn TurnMiddleware>> =
            vec![Arc::new(DenyNamed("rm")), Arc::new(RequireRedacted)];
        let outcome = block_on(before_tool_chain(
            &chain,
            &call("rm", json!({})),
            None,
            &ToolContext::default(),
        ));
        assert_eq!(
            outcome,
            ToolChainOutcome::Deny {
                reason: "`rm` is not allowed".into()
            }
        );
    }

    #[test]
    fn an_untouched_call_is_not_marked_rewritten() {
        let chain: Vec<Arc<dyn TurnMiddleware>> = vec![Arc::new(DenyNamed("other"))];
        let outcome = block_on(before_tool_chain(
            &chain,
            &call("read", json!({})),
            None,
            &ToolContext::default(),
        ));
        assert!(matches!(
            outcome,
            ToolChainOutcome::Proceed {
                rewritten: false,
                ..
            }
        ));
    }

    #[test]
    fn after_tool_threads_output_through_in_order() {
        let chain: Vec<Arc<dyn TurnMiddleware>> =
            vec![Arc::new(Suffix("-a")), Arc::new(Suffix("-b"))];
        let output = block_on(after_tool_chain(
            &chain,
            &call("read", json!({})),
            None,
            &ToolContext::default(),
            ToolOutput::text("base"),
        ));
        assert_eq!(output.content, "base-a-b");
    }
}
