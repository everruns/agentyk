//! Everruns-style memory and compaction over the [`ContextAssembler`] seam.
//!
//! A long-running everruns session keeps a persistent "memory" note and, past a
//! point, stops replaying the whole transcript to the model. Both are shaping
//! *what the turn sends*, not the turn machine — exactly what
//! [`agentyk_core::context::ContextAssembler`] is for. [`MemoryAssembler`]
//! implements it with **no core change**: it prepends a memory note and,
//! optionally, keeps only the most recent turns.
//!
//! The full, untrimmed history still lives in the event log — this is a
//! per-turn *view*, not a mutation of what's recorded.

use agentyk_core::context::{ContextAssembler, ContextAssembly, ContextRequest};
use agentyk_core::message::Message;
use async_trait::async_trait;

/// Injects a persistent memory note into every turn, and can cap how much
/// history is replayed (a minimal stand-in for compaction).
///
/// Built over `agentyk-core`'s public [`ContextAssembler`] — the everruns
/// memory/compaction behavior needs no new core surface.
#[derive(Debug, Clone)]
pub struct MemoryAssembler {
    memory: String,
    keep_last: Option<usize>,
}

impl MemoryAssembler {
    /// A memory note prepended to what every turn sends. Full history is
    /// replayed after it unless [`keep_last`](Self::keep_last) trims it.
    pub fn new(memory: impl Into<String>) -> Self {
        Self {
            memory: memory.into(),
            keep_last: None,
        }
    }

    /// Replay at most the `n` most recent history messages after the memory
    /// note — the compaction knob. Without this, the whole history is sent.
    pub fn keep_last(mut self, n: usize) -> Self {
        self.keep_last = Some(n);
        self
    }

    /// The memory note as a message. A `user`-role reminder keeps it inside
    /// the public constructor surface (core exposes no `system` builder) while
    /// still landing ahead of the replayed turns.
    fn note(&self) -> Message {
        Message::user(format!("(memory) {}", self.memory))
    }
}

#[async_trait]
impl ContextAssembler for MemoryAssembler {
    async fn assemble(&self, request: ContextRequest<'_>) -> agentyk_core::Result<ContextAssembly> {
        let messages = request.messages;
        let start = match self.keep_last {
            Some(n) => messages.len().saturating_sub(n),
            None => 0,
        };
        let mut out = Vec::with_capacity(1 + messages.len() - start);
        out.push(self.note());
        out.extend_from_slice(&messages[start..]);
        Ok(ContextAssembly::new(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Drive a never-suspending assembler future to completion without a
    /// runtime — core carries no async runtime, and neither does this lib.
    fn assemble(assembler: &MemoryAssembler, messages: &[Message]) -> Vec<Message> {
        let session_id = agentyk_core::SessionId::new();
        let model = agentyk_core::ModelSpec::llmsim();
        let events = agentyk_core::InMemoryEventLog::new();
        let future = assembler.assemble(ContextRequest {
            point: agentyk_core::SessionPoint::new(session_id, messages.len() as u64),
            turn_id: agentyk_core::TurnId::new(),
            iteration: 1,
            model: &model,
            token_limit: None,
            messages,
            events: &events,
        });
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                break out.unwrap().messages;
            }
        }
    }

    #[test]
    fn prepends_memory_note_and_keeps_full_history() {
        let history = vec![Message::user("one"), Message::assistant("two")];
        let out = assemble(&MemoryAssembler::new("user prefers metric"), &history);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].text(), "(memory) user prefers metric");
        assert_eq!(out[1].text(), "one");
        assert_eq!(out[2].text(), "two");
    }

    #[test]
    fn keep_last_trims_history_but_keeps_the_note() {
        let history = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
        ];
        let out = assemble(&MemoryAssembler::new("remember X").keep_last(1), &history);

        // Note + only the single most recent history message.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text(), "(memory) remember X");
        assert_eq!(out[1].text(), "three");
    }

    #[test]
    fn keep_last_larger_than_history_is_a_noop_trim() {
        let history = vec![Message::user("one")];
        let out = assemble(&MemoryAssembler::new("m").keep_last(10), &history);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].text(), "one");
    }
}
