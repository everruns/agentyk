//! Scripted offline driver (everruns: `llmsim`) for deterministic examples
//! and tests — no API key, no network.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::driver::{ChatDriver, ChatRequest, ChatResponse, DriverId, Usage};
use crate::error::Result;
use crate::message::{Message, ToolCall};

#[derive(Debug, Clone)]
pub struct SimToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// One scripted completion. If `tool_calls` is non-empty the simulated model
/// requests those tools; otherwise it answers with `text`.
#[derive(Debug, Clone, Default)]
pub struct SimTurn {
    pub text: String,
    pub tool_calls: Vec<SimToolCall>,
}

impl SimTurn {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            text: String::new(),
            tool_calls: vec![SimToolCall {
                name: name.into(),
                arguments,
            }],
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
    fn id(&self) -> DriverId {
        DriverId::llmsim()
    }

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

        Ok(ChatResponse {
            message,
            usage: Usage::default(),
        })
    }
}
