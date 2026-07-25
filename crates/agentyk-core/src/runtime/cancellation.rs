//! Cooperative turn cancellation.
//!
//! A [`CancellationToken`] is a cheap, cloneable, thread-safe flag: hold a
//! clone, call [`CancellationToken::cancel`] from anywhere (another task, a
//! signal handler, a UI event), and any executor checking
//! [`CancellationToken::is_cancelled`] stops at its next check point.
//!
//! This is intentionally std-only — no tokio dependency — so it belongs in
//! `agentyk-core` and works the same way in an in-process executor, a
//! durable host, or a synchronous test harness. Cancellation is
//! **cooperative**: it does not preempt an in-flight LLM call or tool
//! execution, it stops the *next* one. The one exception is streaming: since
//! a [`crate::driver::DeltaSink`] is polled once per chunk, an executor can
//! check the token there too and abort mid-stream — see
//! `agentyk`'s `InProcessExecutor`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cancellation signal shared between a caller and a running turn.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled_and_is_shared_across_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        assert!(!clone.is_cancelled());

        clone.cancel();
        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
    }

    #[test]
    fn independent_tokens_do_not_share_state() {
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        a.cancel();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
    }
}
