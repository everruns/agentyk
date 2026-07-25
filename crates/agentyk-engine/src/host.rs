//! Per-run host context and the single event recording path.

use chrono::Utc;

use agentyk_core::cancellation::CancellationToken;
use agentyk_core::driver::{ModelSpec, Usage};
use agentyk_core::error::Result;
use agentyk_core::event::{EventData, EventRequest};
use agentyk_core::event_log::{EventLog, ExpectedVersion};
use agentyk_core::id::{EventId, SessionId, TurnId};
use agentyk_core::replay::History;

use crate::agent::{AgentDefinition, AgentEnvironment};
/// The outcome of one executed turn.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TurnResult {
    /// Correlates with every event this turn recorded.
    pub turn_id: TurnId,
    /// The model's final text response.
    pub response: String,
    /// LLM completions performed within the turn.
    pub iterations: usize,
    /// Tool calls executed within the turn.
    pub tool_calls: usize,
    /// Tokens spent across every completion in the turn.
    pub usage: Usage,
}

impl TurnResult {
    /// Record how a turn ended.
    pub fn new(
        turn_id: TurnId,
        response: impl Into<String>,
        iterations: usize,
        tool_calls: usize,
        usage: Usage,
    ) -> Self {
        Self {
            turn_id,
            response: response.into(),
            iterations,
            tool_calls,
            usage,
        }
    }
}

/// Agent composition plus the session-specific resources for one turn.
#[non_exhaustive]
pub struct TurnHost<'a> {
    /// Session being advanced.
    pub session_id: SessionId,
    /// Valid behavioral definition being interpreted.
    pub definition: &'a AgentDefinition,
    /// Shared execution resources.
    pub environment: &'a AgentEnvironment,
    /// Effective model after per-turn controls.
    pub model: &'a ModelSpec,
    /// Durable event storage.
    pub log: &'a dyn EventLog,
    /// Live projection of durable message history.
    pub history: &'a mut History,
    /// Cooperative cancellation signal.
    pub cancellation: CancellationToken,
}

impl<'a> TurnHost<'a> {
    /// Construct a host context using the agent's default model.
    pub fn new(
        session_id: SessionId,
        definition: &'a AgentDefinition,
        environment: &'a AgentEnvironment,
        log: &'a dyn EventLog,
        history: &'a mut History,
    ) -> Self {
        Self {
            session_id,
            definition,
            environment,
            model: &definition.model,
            log,
            history,
            cancellation: CancellationToken::new(),
        }
    }

    /// Override the effective model for this turn.
    pub fn model(mut self, model: &'a ModelSpec) -> Self {
        self.model = model;
        self
    }

    /// Attach the cooperative cancellation signal.
    pub fn cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = token;
        self
    }

    /// Persist durable effects atomically, then advance projections and
    /// notify observers. Ephemeral effects bypass storage.
    pub async fn record(&mut self, turn_id: TurnId, effects: Vec<EventData>) -> Result<()> {
        let mut durable = Vec::new();
        let mut ephemeral = Vec::new();
        for data in effects {
            let request = EventRequest::with_turn(self.session_id, turn_id, data);
            if request.data.is_ephemeral() {
                ephemeral.push(request.into_ephemeral_event(EventId::new(), Utc::now()));
            } else {
                durable.push(request);
            }
        }

        let recorded = if durable.is_empty() {
            Vec::new()
        } else {
            let version = self
                .log
                .read_after(self.session_id, None)
                .await?
                .last()
                .and_then(|event| event.sequence)
                .unwrap_or(0);
            self.log
                .append_batch(self.session_id, ExpectedVersion::Exact(version), durable)
                .await?
        };

        for event in recorded.into_iter().chain(ephemeral) {
            if event.sequence.is_some() {
                self.history.apply(&event.data);
            }
            for listener in &self.environment.listeners {
                listener.on_event(&event).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    use agentyk_core::context::PassthroughContextAssembler;
    use agentyk_core::driver::{DriverRegistry, ModelSpec};
    use agentyk_core::event::{Event, EventRequest};
    use agentyk_core::event_log::EventStore;
    use agentyk_core::extensions::Extensions;
    use agentyk_core::id::MessageId;

    use super::*;
    use crate::agent::{AgentDefinition, AgentEnvironment};

    struct RejectingStore;

    #[async_trait::async_trait]
    impl EventStore for RejectingStore {
        async fn append_batch(
            &self,
            _session_id: SessionId,
            _expected: ExpectedVersion,
            _requests: Vec<EventRequest>,
        ) -> Result<Vec<Event>> {
            panic!("ephemeral events must not reach storage")
        }

        async fn read_after(
            &self,
            _session_id: SessionId,
            _sequence: Option<u64>,
        ) -> Result<Vec<Event>> {
            panic!("ephemeral events must not read storage")
        }
    }

    struct ConflictingStore;

    #[async_trait::async_trait]
    impl EventStore for ConflictingStore {
        async fn append_batch(
            &self,
            _session_id: SessionId,
            _expected: ExpectedVersion,
            _requests: Vec<EventRequest>,
        ) -> Result<Vec<Event>> {
            Err(agentyk_core::error::Error::EventConflict {
                expected: 0,
                actual: 1,
            })
        }

        async fn read_after(
            &self,
            _session_id: SessionId,
            _sequence: Option<u64>,
        ) -> Result<Vec<Event>> {
            Ok(Vec::new())
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    #[test]
    fn ephemeral_events_bypass_the_store_entirely() {
        let definition = AgentDefinition {
            name: "test".into(),
            instructions: String::new(),
            model: ModelSpec::llmsim(),
            capabilities: Vec::new(),
            max_iterations: 4,
            middleware: Vec::new(),
            budget_checker: None,
            context_assembler: Arc::new(PassthroughContextAssembler),
        };
        let environment = AgentEnvironment {
            drivers: DriverRegistry::new(),
            listeners: Vec::new(),
            extensions: Extensions::new(),
        };
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut history = History::new();
        let mut host = TurnHost::new(
            session_id,
            &definition,
            &environment,
            &RejectingStore,
            &mut history,
        );

        block_on(host.record(
            turn_id,
            vec![EventData::OutputMessageDelta {
                message_id: MessageId::new(),
                delta: "a".into(),
                accumulated: "a".into(),
            }],
        ))
        .unwrap();
    }

    #[test]
    fn failed_append_does_not_advance_history() {
        let definition = AgentDefinition {
            name: "test".into(),
            instructions: String::new(),
            model: ModelSpec::llmsim(),
            capabilities: Vec::new(),
            max_iterations: 4,
            middleware: Vec::new(),
            budget_checker: None,
            context_assembler: Arc::new(PassthroughContextAssembler),
        };
        let environment = AgentEnvironment {
            drivers: DriverRegistry::new(),
            listeners: Vec::new(),
            extensions: Extensions::new(),
        };
        let session_id = SessionId::new();
        let mut history = History::new();
        let mut host = TurnHost::new(
            session_id,
            &definition,
            &environment,
            &ConflictingStore,
            &mut history,
        );

        let error = block_on(host.record(
            TurnId::new(),
            vec![EventData::InputMessage {
                message: agentyk_core::message::Message::user("not durable"),
            }],
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            agentyk_core::error::Error::EventConflict { .. }
        ));
        assert!(history.messages().is_empty());
    }
}
