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
use serde::{Deserialize, Serialize};

use crate::driver::ModelSpec;
use crate::error::Result;
use crate::event::EventData;
use crate::event_log::{EventStore, SessionPoint};
use crate::id::TurnId;
use crate::message::Message;

/// Everything a context policy needs to shape one model request.
pub struct ContextRequest<'a> {
    /// Immutable durable head whose history is being assembled.
    pub point: SessionPoint,
    /// Turn about to invoke the model.
    pub turn_id: TurnId,
    /// One-based reason iteration within the turn.
    pub iteration: usize,
    /// Effective model after per-turn overrides.
    pub model: &'a ModelSpec,
    /// Optional input ceiling configured by the application.
    pub token_limit: Option<usize>,
    /// Complete replayed history at `point`.
    pub messages: &'a [Message],
    /// Paged access to the authoritative stream for policies that maintain
    /// their own summaries, indexes, or historical windows.
    pub events: &'a dyn EventStore,
}

/// Provenance for one contribution to assembled model context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSource {
    /// Human-readable source family such as `history`, `summary`, or `memory`.
    pub kind: String,
    /// Optional first durable point represented by this contribution.
    pub from: Option<SessionPoint>,
    /// Optional last durable point represented by this contribution.
    pub through: Option<SessionPoint>,
}

/// Durable observational event produced while assembling context.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEvent {
    /// Dot-notation event type such as `context.compacted`.
    pub event_type: String,
    /// Context-policy-owned event data.
    pub payload: serde_json::Value,
}

/// Model messages plus the accounting and provenance behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextAssembly {
    /// Messages sent to the provider.
    pub messages: Vec<Message>,
    /// Optional estimated provider input tokens.
    pub estimated_tokens: Option<usize>,
    /// Contributions used to construct `messages`.
    pub sources: Vec<ContextSource>,
    /// Observational events the engine persists before invoking the model.
    pub events: Vec<ContextEvent>,
}

impl ContextAssembly {
    /// An assembly containing only `messages`.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            estimated_tokens: None,
            sources: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Split provider messages from the observational events the engine must
    /// persist before invoking the model.
    pub fn into_messages_and_events(self) -> (Vec<Message>, Vec<EventData>) {
        let mut events = Vec::new();
        if self.estimated_tokens.is_some() || !self.sources.is_empty() {
            events.push(EventData::custom(
                "context.assembled",
                serde_json::json!({
                    "estimated_tokens": self.estimated_tokens,
                    "sources": self.sources,
                }),
            ));
        }
        events.extend(
            self.events
                .into_iter()
                .map(|event| EventData::custom(event.event_type, event.payload)),
        );
        (self.messages, events)
    }
}

/// Decides what a turn actually sends to the model.
///
/// Sits between replay and the reason atom, so trimming, compaction and
/// memory injection are implementations of this seam rather than changes to
/// the turn machine — and none of them touch the event log, which keeps the
/// real history.
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    /// Turn replayed history into what this turn actually sends to the
    /// model.
    ///
    /// This is where trimming, compaction and memory injection belong: the
    /// event log keeps the real history, and only what this returns reaches
    /// the provider. Called once per reason step, so a resumed session gets
    /// the same treatment as a fresh one.
    /// The request carries the immutable history point, effective model,
    /// iteration, optional token ceiling, and full replayed messages. The
    /// result may report accounting, provenance, and observational events in
    /// addition to the provider-facing messages.
    async fn assemble(&self, request: ContextRequest<'_>) -> Result<ContextAssembly>;
}

/// Sends the full history unchanged — the default.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughContextAssembler;

#[async_trait]
impl ContextAssembler for PassthroughContextAssembler {
    async fn assemble(&self, request: ContextRequest<'_>) -> Result<ContextAssembly> {
        Ok(ContextAssembly {
            messages: request.messages.to_vec(),
            estimated_tokens: None,
            sources: Vec::new(),
            events: Vec::new(),
        })
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
        let model = ModelSpec::llmsim();
        let session_id = crate::id::SessionId::new();
        let events = crate::event_log::InMemoryEventLog::new();
        let future = assembler.assemble(ContextRequest {
            point: SessionPoint::new(session_id, 2),
            turn_id: TurnId::new(),
            iteration: 1,
            model: &model,
            token_limit: None,
            messages: &messages,
            events: &events,
        });
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let result = loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                break output;
            }
        };
        assert_eq!(result.unwrap().messages, messages);
    }
}
