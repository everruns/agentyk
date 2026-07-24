//! # agentyk-everruns (prototype)
//!
//! A **proof of the extensibility boundary**: everruns-flavored behavior built
//! as a satellite over [`agentyk-core`](agentyk_core)'s public seams, with no
//! change to core. See `docs/extensibility.md` in the agentyk repo.
//!
//! The library depends on `agentyk-core` **only** — no framework, no tokio, no
//! HTTP. It shows two of the design's claims end-to-end.
//!
//! **Behavior lives in a custom [`TurnExecutor`](agentyk_core::executor::TurnExecutor).**
//! [`EverrunsExecutor`] drives the same [`TurnState`](agentyk_core::turn::TurnState)
//! and [`atoms`](agentyk_core::atoms) as the built-in executor, but adds
//! hint-based tool **approval** — the everruns guardrail shape (deny with a
//! user-facing message) that core's `PreToolUseHook` can't express. This is the
//! intended home for adoption "gap 4".
//!
//! **Everruns-flavored data rides the `metadata` hatch.** [`ToolHints`]
//! (`readonly`/`destructive`/`open_world`) live in
//! [`ToolDefinition::metadata`](agentyk_core::tool::ToolDefinition::metadata)
//! under a `"hints"` key — core never learns the schema; this crate owns it.
//!
//! ```no_run
//! use agentyk_everruns::{EverrunsExecutor, HintedTool, ToolHints, Approver, ApprovalDecision};
//! # use agentyk_core::message::ToolCall;
//! # async fn demo(deny_all: impl Approver + 'static) {
//! // Attach a custom executor that gates risky tools:
//! let executor = EverrunsExecutor::new(deny_all);
//! // Tag a tool as destructive so the executor gates it:
//! // let tool = HintedTool::new(my_delete_tool, ToolHints::destructive());
//! # let _ = (executor,);
//! # }
//! ```

mod executor;
mod hints;
mod narration;

pub use executor::{
    AllowAll, ApprovalDecision, Approver, EverrunsExecutor, GuardOutcome, PreToolGuard,
};
pub use hints::{HintedTool, ToolHints};
pub use narration::NarrationListener;
