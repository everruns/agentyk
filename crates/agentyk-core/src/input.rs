//! Steering — input that arrives while a turn is already running.
//!
//! A turn can take minutes. In that time the person who started it learns
//! things: the approach is wrong, the file was already fixed, stop after this
//! test. Without somewhere to put that, their only options are to wait or to
//! cancel and start over, losing everything the turn has done.
//!
//! An [`InputQueue`] is that somewhere. A caller holds a clone, pushes a
//! message whenever it likes, and the executor drains it **between reasoning
//! steps** — see [`InputQueue::drain`] for why that boundary and not any
//! other. Drained messages are recorded as ordinary `input.message` events, so
//! they enter history exactly like the message that opened the turn and a
//! replay reconstructs the same conversation.
//!
//! Deliberately std-only (a `Mutex<VecDeque>`, no channel crate) so it lives
//! in `agentyk-core` and works the same in any host.
//!
//! ```
//! use agentyk_core::{input::InputQueue, message::Message};
//!
//! let queue = InputQueue::new();
//! let steering = queue.clone();
//! steering.push(Message::user("actually, skip the tests"));
//! assert_eq!(queue.drain().len(), 1);
//! assert!(queue.drain().is_empty(), "draining consumes");
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::message::Message;

/// Messages waiting to join a turn already in progress. Cheap to clone;
/// every clone shares one queue.
#[derive(Clone, Default)]
pub struct InputQueue(Arc<Mutex<VecDeque<Message>>>);

impl InputQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a message for the running turn to pick up at its next reasoning
    /// step. Never blocks and never fails: a queue with no turn draining it
    /// simply holds what it was given, and the next turn takes it.
    pub fn push(&self, message: Message) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(message);
    }

    /// Whether anything is waiting. Cheaper than draining for a host that
    /// only wants to know.
    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    /// Take everything queued, in the order it was pushed.
    ///
    /// Executors call this **only when the next action is a reasoning step**,
    /// never mid-batch. That is a protocol constraint, not a preference:
    /// providers require every tool call to be answered by its result, so a
    /// user message wedged between a `tool_use` and its `tool_result` makes
    /// the exchange invalid. Steering therefore lands at the next model call —
    /// which is also the first moment the model could act on it anyway.
    pub fn drain(&self) -> Vec<Message> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect()
    }
}

impl std::fmt::Debug for InputQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputQueue")
            .field("queued", &self.0.lock().map(|q| q.len()).unwrap_or(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_queue() {
        let queue = InputQueue::new();
        let handle = queue.clone();
        assert!(queue.is_empty());

        handle.push(Message::user("first"));
        handle.push(Message::user("second"));
        assert!(!queue.is_empty());

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text(), "first");
        assert_eq!(drained[1].text(), "second");
        assert!(handle.is_empty(), "draining one clone drains them all");
    }

    #[test]
    fn a_queue_nobody_is_draining_keeps_its_messages() {
        let queue = InputQueue::new();
        queue.push(Message::user("held"));
        assert_eq!(queue.drain().len(), 1);
    }
}
