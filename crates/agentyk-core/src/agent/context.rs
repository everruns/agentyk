//! The context-assembly seam — what actually gets sent to the model, as
//! opposed to what's in the log.
//!
//! By default a turn sends its *entire* replayed history to the model every
//! time. That's correct and simple, but a long-running session eventually
//! wants something smarter — trimming, summarization/compaction, memory
//! injection. [`ContextAssembler`] is where that lives: it sits between
//! replay and [`crate::atoms::reason`], transforming the full history into
//! whatever this turn actually sends. [`PassthroughContextAssembler`] (the
//! default — see `AgentBuilder::context_assembler` in the framework crate)
//! does nothing; compaction and friends are meant to be *implementations*
//! of this trait, not changes to the turn machine.

use async_trait::async_trait;

use crate::id::SessionId;
use crate::message::Message;

#[async_trait]
pub trait ContextAssembler: Send + Sync {
    /// `messages` is the full replayed history. Return what this turn
    /// should actually send to the model.
    async fn assemble(&self, session_id: SessionId, messages: &[Message]) -> Vec<Message>;
}

/// Sends the full history unchanged — the default.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughContextAssembler;

#[async_trait]
impl ContextAssembler for PassthroughContextAssembler {
    async fn assemble(&self, _session_id: SessionId, messages: &[Message]) -> Vec<Message> {
        messages.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_returns_history_unchanged() {
        let messages = vec![Message::user("hi"), Message::assistant("hello")];
        let assembler = PassthroughContextAssembler;

        // core has no async runtime dependency; a bare poll suffices since
        // this implementation never suspends.
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        let future = assembler.assemble(SessionId::new(), &messages);
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let result = loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                break output;
            }
        };
        assert_eq!(result, messages);
    }
}
