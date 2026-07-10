//! Session — a conversation with an agent, driven by the turn loop.
//!
//! `Session::run` executes one turn of the everruns `input → reason → act`
//! contract: record the input, ask the model, execute any requested tools,
//! feed results back, and repeat until the model answers in text (or the
//! iteration ceiling trips). Every step is appended to the [`EventLog`] and
//! fanned out to listeners; a session can be [resumed](crate::Agent::resume_session)
//! from its log alone.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::Agent;
use crate::capability::SystemPromptContext;
use crate::driver::{ChatRequest, Usage};
use crate::error::{Error, Result};
use crate::event::{Event, EventData, EventRequest};
use crate::event_log::EventLog;
use crate::id::{SessionId, TurnId};
use crate::message::Message;
use crate::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};

/// The outcome of one [`Session::run`] call.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub turn_id: TurnId,
    /// The model's final text response.
    pub response: String,
    /// LLM completions performed within the turn.
    pub iterations: usize,
    /// Tool calls executed within the turn.
    pub tool_calls: usize,
    pub usage: Usage,
}

pub struct Session {
    agent: Agent,
    id: SessionId,
    log: Arc<dyn EventLog>,
    messages: Vec<Message>,
}

impl Session {
    pub(crate) fn new(agent: Agent, log: Arc<dyn EventLog>) -> Self {
        Self {
            agent,
            id: SessionId::new(),
            log,
            messages: Vec::new(),
        }
    }

    pub(crate) async fn resume(
        agent: Agent,
        log: Arc<dyn EventLog>,
        session_id: SessionId,
    ) -> Result<Self> {
        let events = log.read(session_id).await?;
        let messages = messages_from_events(&events);
        Ok(Self {
            agent,
            id: session_id,
            log,
            messages,
        })
    }

    /// The session's correlation id — generated internally, readable on
    /// demand, never an input you must mint.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The reconstructed-or-live message history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// All events recorded for this session, ordered by sequence.
    pub async fn events(&self) -> Result<Vec<Event>> {
        self.log.read(self.id).await
    }

    async fn emit(&self, turn_id: TurnId, data: EventData) -> Result<Event> {
        let event = self
            .log
            .append(EventRequest::with_turn(self.id, turn_id, data))
            .await?;
        for listener in &self.agent.inner.listeners {
            listener.on_event(&event).await;
        }
        Ok(event)
    }

    /// Run one turn: user input in, final assistant text out.
    pub async fn run(&mut self, input: impl Into<String>) -> Result<TurnResult> {
        let agent = self.agent.inner.clone();
        let turn_id = TurnId::new();

        self.emit(turn_id, EventData::TurnStarted).await?;
        let user_message = Message::user(input);
        self.emit(
            turn_id,
            EventData::InputMessage {
                message: user_message.clone(),
            },
        )
        .await?;
        self.messages.push(user_message);

        // Resolve tools from capabilities (fresh each turn — capabilities may
        // discover tools dynamically, e.g. MCP).
        let mut tool_index: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut tool_definitions: Vec<ToolDefinition> = Vec::new();
        for capability in &agent.capabilities {
            for tool in capability.tools().await? {
                let definition = tool.definition();
                tool_index.insert(definition.name.clone(), tool);
                tool_definitions.push(definition);
            }
        }

        // Assemble the system prompt: agent prompt + capability contributions.
        let prompt_context = SystemPromptContext {
            session_id: self.id,
        };
        let mut prompt_parts: Vec<String> = Vec::new();
        if !agent.system_prompt.is_empty() {
            prompt_parts.push(agent.system_prompt.clone());
        }
        for capability in &agent.capabilities {
            if let Some(contribution) = capability.system_prompt_contribution(&prompt_context).await
            {
                prompt_parts.push(contribution);
            }
        }
        let system_prompt = if prompt_parts.is_empty() {
            None
        } else {
            Some(prompt_parts.join("\n\n"))
        };

        let driver = agent
            .drivers
            .get(&agent.model.driver)
            .ok_or_else(|| Error::UnknownDriver(agent.model.driver.to_string()))?;

        let mut usage = Usage::default();
        let mut executed_tool_calls = 0usize;

        for iteration in 1..=agent.max_iterations {
            let request = ChatRequest {
                model: agent.model.clone(),
                system_prompt: system_prompt.clone(),
                messages: self.messages.clone(),
                tools: tool_definitions.clone(),
            };

            let response = match driver.complete(request).await {
                Ok(response) => response,
                Err(error) => {
                    self.emit(
                        turn_id,
                        EventData::TurnFailed {
                            error: error.to_string(),
                        },
                    )
                    .await?;
                    return Err(error);
                }
            };
            usage.add(response.usage);

            let assistant_message = response.message;
            self.emit(
                turn_id,
                EventData::OutputMessage {
                    message: assistant_message.clone(),
                },
            )
            .await?;
            self.messages.push(assistant_message.clone());

            if assistant_message.tool_calls.is_empty() {
                self.emit(
                    turn_id,
                    EventData::TurnCompleted {
                        iterations: iteration,
                        tool_calls: executed_tool_calls,
                    },
                )
                .await?;
                return Ok(TurnResult {
                    turn_id,
                    response: assistant_message.content,
                    iterations: iteration,
                    tool_calls: executed_tool_calls,
                    usage,
                });
            }

            let tool_context = ToolContext {
                session_id: self.id,
                turn_id,
            };
            for call in &assistant_message.tool_calls {
                self.emit(turn_id, EventData::ToolStarted { call: call.clone() })
                    .await?;
                let output = match tool_index.get(&call.name) {
                    Some(tool) => tool.execute(call.arguments.clone(), &tool_context).await,
                    None => ToolOutput::error(format!("unknown tool `{}`", call.name)),
                };
                executed_tool_calls += 1;
                self.emit(
                    turn_id,
                    EventData::ToolCompleted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        output: output.content.clone(),
                        is_error: output.is_error,
                    },
                )
                .await?;
                self.messages
                    .push(Message::tool_result(call.id.clone(), output.content));
            }
        }

        let error = Error::MaxIterations(agent.max_iterations);
        self.emit(
            turn_id,
            EventData::TurnFailed {
                error: error.to_string(),
            },
        )
        .await?;
        Err(error)
    }
}

/// Rebuild message history from an event log — the replay half of the
/// persistence story.
pub(crate) fn messages_from_events(events: &[Event]) -> Vec<Message> {
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|e| e.sequence);
    let mut messages = Vec::new();
    for event in ordered {
        match &event.data {
            EventData::InputMessage { message } | EventData::OutputMessage { message } => {
                messages.push(message.clone());
            }
            EventData::ToolCompleted {
                call_id, output, ..
            } => {
                messages.push(Message::tool_result(call_id.clone(), output.clone()));
            }
            _ => {}
        }
    }
    messages
}
