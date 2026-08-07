//! Scripted offline driver (everruns: `llmsim`) for deterministic examples
//! and tests — no API key, no network.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use agentyk_core::driver::{ChatDriver, ChatRequest, ChatResponse, Usage};
use agentyk_core::error::Result;
use agentyk_core::message::{Message, ToolCall};

#[derive(Debug, Clone)]
#[non_exhaustive]
/// One tool call for the simulated model to request.
pub struct SimToolCall {
    /// The tool to ask for. It need not exist — an unknown tool is a good
    /// thing to test.
    pub name: String,
    /// The arguments to ask with.
    pub arguments: serde_json::Value,
}

impl SimToolCall {
    /// Script one tool call.
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

/// One scripted completion. If `tool_calls` is non-empty the simulated model
/// requests those tools; otherwise it answers with `text`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SimTurn {
    /// The text to answer with.
    pub text: String,
    /// Tools to request instead of answering. Non-empty wins over `text`.
    pub tool_calls: Vec<SimToolCall>,
}

impl SimTurn {
    /// A turn that answers with text, ending the loop.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }

    /// A turn that requests one tool, continuing the loop.
    pub fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            text: String::new(),
            tool_calls: vec![SimToolCall::new(name, arguments)],
        }
    }

    /// A turn requesting several tools at once — what a parallel-capable
    /// executor fans out as one batch.
    pub fn tool_calls(calls: impl IntoIterator<Item = SimToolCall>) -> Self {
        Self {
            text: String::new(),
            tool_calls: calls.into_iter().collect(),
        }
    }
}

/// Pops one [`SimTurn`] per completion request and records every request it
/// receives (for assertions in tests).
pub struct SimDriver {
    turns: Mutex<VecDeque<SimTurn>>,
    requests: Mutex<Vec<ChatRequest>>,
    call_counter: AtomicU64,
}

impl SimDriver {
    /// A driver that plays these turns in order. Once the script runs out
    /// it answers with a marker string rather than hanging, so an
    /// under-specified test fails visibly.
    pub fn new(turns: impl IntoIterator<Item = SimTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            call_counter: AtomicU64::new(0),
        }
    }

    /// Every request this driver has served, in order.
    pub fn recorded_requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("sim driver poisoned").clone()
    }
}

#[async_trait]
impl ChatDriver for SimDriver {
    async fn complete(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.requests
            .lock()
            .expect("sim driver poisoned")
            .push(request);
        let turn = self
            .turns
            .lock()
            .expect("sim driver poisoned")
            .pop_front()
            .unwrap_or_else(|| SimTurn::text("(llmsim: script exhausted)"));

        let message = if turn.tool_calls.is_empty() {
            Message::assistant(turn.text)
        } else {
            let calls = turn
                .tool_calls
                .into_iter()
                .map(|c| ToolCall {
                    id: format!("call_{}", self.call_counter.fetch_add(1, Ordering::Relaxed)),
                    name: c.name,
                    arguments: c.arguments,
                })
                .collect();
            Message::assistant_with_calls(turn.text, calls)
        };

        Ok(ChatResponse::new(message, Usage::default()))
    }
}
