//! # agentyk-everruns-poc (prototype)
//!
//! A **proof of the extensibility boundary**: everruns-flavored behavior built
//! as a satellite over [`agentyk-core`](agentyk_core)'s public seams, with no
//! change to core. See `knowledge/extensibility.md` in the agentyk repo.
//!
//! The library depends on `agentyk-core` **only** — no framework, no tokio, no
//! HTTP. It shows the design's claims end-to-end: behavior in a custom
//! executor, everruns data on the `metadata` hatch, and memory/compaction over
//! the context-assembly seam.
//!
//! **Behavior lives in a custom [`TurnExecutor`](agentyk_core::executor::TurnExecutor).**
//! [`EverrunsExecutor`] drives the same [`TurnState`](agentyk_core::turn::TurnState)
//! and [`atoms`](agentyk_core::atoms) as the built-in executor, differing only
//! in **dispatch strategy**: it fans a tool batch out concurrently. Guard
//! policy is not part of it — hint-based **approval** is
//! [`ApprovalMiddleware`], ordinary core
//! [`TurnMiddleware`](agentyk_core::middleware::TurnMiddleware) attached with
//! `AgentBuilder::middleware`. Denial and rewriting stopped needing a forked
//! act loop once core middleware could express both.
//!
//! **Everruns-flavored data rides the `metadata` hatch.** [`ToolHints`]
//! (`readonly`/`destructive`/`open_world`) live in
//! [`ToolDefinition::metadata`](agentyk_core::tool::ToolDefinition::metadata)
//! under a `"hints"` key — core never learns the schema; this crate owns it.
//!
//! ```no_run
//! use agentyk_everruns_poc::{ApprovalMiddleware, EverrunsExecutor, HintedTool, ToolHints, Approver, ApprovalDecision};
//! # use agentyk_core::message::ToolCall;
//! # async fn demo(deny_all: impl Approver + 'static) {
//! // Concurrent dispatch, plus approval as ordinary middleware:
//! let executor = EverrunsExecutor;
//! let approval = ApprovalMiddleware::new(deny_all);
//! // Tag a tool as destructive so the executor gates it:
//! // let tool = HintedTool::new(my_delete_tool, ToolHints::destructive());
//! # let _ = (executor, approval);
//! # }
//! ```

mod executor;
mod hints;
mod memory;
mod narration;

pub use executor::{AllowAll, ApprovalDecision, ApprovalMiddleware, Approver, EverrunsExecutor};
pub use hints::{HintedTool, ToolHints};
pub use memory::MemoryAssembler;
pub use narration::NarrationListener;
