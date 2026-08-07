//! Ready-made [`Provider`](agentyk_core::provider::Provider)s for the
//! services agentyk bundles a driver for.
//!
//! Each is a two-line assembly — endpoint, credentials, and the driver whose
//! protocol the service speaks — that you could equally write yourself with
//! [`Provider::new`](agentyk_core::provider::Provider::new). They live here
//! rather than as constructors on `Provider` because `Provider` is defined in
//! `agentyk-core`, which deliberately knows nothing about HTTP.
//!
//! For a service without one — a gateway, a local runtime, an
//! OpenAI-compatible vendor — build it directly:
//!
//! ```
//! use agentyk::{OpenAiDriver, Provider};
//!
//! let ollama = Provider::new("ollama", OpenAiDriver::new()).base_url("http://localhost:11434/v1");
//! ```

pub use crate::drivers::anthropic::anthropic;
pub use crate::drivers::openai::openai;
