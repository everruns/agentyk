//! # agentyk-everruns-poc (prototype)
//!
//! A **proof of the extensibility boundary**: everruns-flavored behavior built
//! as a satellite over [`agentyk-core`](agentyk_core)'s public seams, with no
//! change to core. See `specs/extensibility.md` in the agentyk repo.
//!
//! The library depends on `agentyk-core` only. It shows that Everruns data can
//! ride the metadata hatch and that approval, memory, and narration compose
//! without replacing the canonical turn engine.
//!
//! Hint-based approval is [`ApprovalMiddleware`], ordinary core
//! [`TurnMiddleware`](agentyk_core::middleware::TurnMiddleware) attached with
//! `AgentBuilder::middleware`.
//!
//! **Everruns-flavored data rides the `metadata` hatch.** [`ToolHints`]
//! (`readonly`/`destructive`/`open_world`) live in
//! [`ToolDefinition::metadata`](agentyk_core::tool::ToolDefinition::metadata)
//! under a `"hints"` key — core never learns the schema; this crate owns it.
//!
//! ```no_run
//! use agentyk_everruns_poc::{ApprovalMiddleware, HintedTool, ToolHints, Approver, ApprovalDecision};
//! # use agentyk_core::message::ToolCall;
//! # async fn demo(deny_all: impl Approver + 'static) {
//! // Approval is ordinary middleware:
//! let approval = ApprovalMiddleware::new(deny_all);
//! // Tag a tool as destructive so the middleware gates it:
//! // let tool = HintedTool::new(my_delete_tool, ToolHints::destructive());
//! # let _ = approval;
//! # }
//! ```

mod approval;
mod hints;
mod memory;
mod narration;

pub use approval::{AllowAll, ApprovalDecision, ApprovalMiddleware, Approver};
pub use hints::{HintedTool, ToolHints};
pub use memory::MemoryAssembler;
pub use narration::NarrationListener;
