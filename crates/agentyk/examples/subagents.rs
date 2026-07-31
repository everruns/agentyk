//! Spawn five child agents, receive task IDs immediately, then wait for every
//! child to finish.
//!
//! The small in-memory task table represents the host-side session-task
//! registry that Everruns supplies in production. Each child is an ordinary
//! by-value Agentyk agent with an independent session and context.
//!
//! ```sh
//! cargo run -p agentyk --example subagents
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentyk::{
    Agent, ModelSpec, Role, SimDriver, SimTurn, Tool, ToolContext, ToolDefinition, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::task::JoinHandle;

type ChildResult = agentyk::Result<String>;

#[derive(Default)]
struct ChildTasks {
    next_task: AtomicU64,
    running: Mutex<HashMap<String, JoinHandle<ChildResult>>>,
}

struct SpawnSubagentsTool {
    tasks: Arc<ChildTasks>,
}

#[async_trait]
impl Tool for SpawnSubagentsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "spawn_subagents",
            "Spawn exactly five independent child agents and return their task IDs immediately.",
            json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 5,
                        "maxItems": 5,
                        "items": {"type": "string"}
                    }
                },
                "required": ["tasks"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        let Some(instructions) = arguments.get("tasks").and_then(|value| value.as_array()) else {
            return ToolOutput::error("tasks must be an array of five strings");
        };
        if instructions.len() != 5 || instructions.iter().any(|value| !value.is_string()) {
            return ToolOutput::error("tasks must contain exactly five strings");
        }

        let mut spawned = Vec::with_capacity(5);
        let mut handles = Vec::with_capacity(5);
        for (index, instruction) in instructions.iter().enumerate() {
            let instruction = instruction.as_str().unwrap_or_default().to_owned();
            let task_id = format!(
                "subagent_{}",
                self.tasks.next_task.fetch_add(1, Ordering::Relaxed) + 1
            );
            let task_id_for_run = task_id.clone();
            let handle = tokio::spawn(async move {
                // Different durations make it visible that all children start
                // before the parent enters its wait.
                tokio::time::sleep(Duration::from_millis(50 * (5 - index) as u64)).await;
                let child = Agent::builder()
                    .name(format!("worker-{}", index + 1))
                    .system_prompt("Complete the assigned independent task.")
                    .model(ModelSpec::llmsim())
                    .driver(SimDriver::new([SimTurn::text(format!(
                        "{task_id_for_run} completed: {instruction}"
                    ))]))
                    .build()?;
                let mut session = child.session();
                let result = session.run(instruction).await?;
                Ok(result.response)
            });
            spawned.push(task_id.clone());
            handles.push((task_id, handle));
        }

        let mut running = self
            .tasks
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (task_id, handle) in handles {
            running.insert(task_id, handle);
        }
        ToolOutput::text(format!("Spawned: {}", spawned.join(", "))).with_metadata(json!({
            "task_ids": spawned,
            "state": "running",
            "kind": "subagent"
        }))
    }
}

struct WaitForSubagentsTool {
    tasks: Arc<ChildTasks>,
}

#[async_trait]
impl Tool for WaitForSubagentsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "wait_for_subagents",
            "Wait until all named subagent tasks finish and return every result.",
            json!({
                "type": "object",
                "properties": {
                    "task_ids": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
                },
                "required": ["task_ids"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        let Some(task_ids) = arguments.get("task_ids").and_then(|value| value.as_array()) else {
            return ToolOutput::error("task_ids must be an array");
        };
        let task_ids: Vec<String> = task_ids
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        if task_ids.len() != 5 {
            return ToolOutput::error("wait_for_subagents expects five task IDs");
        }

        let handles = {
            let mut running = self
                .tasks
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut handles = Vec::with_capacity(task_ids.len());
            for task_id in &task_ids {
                let Some(handle) = running.remove(task_id) else {
                    return ToolOutput::error(format!("unknown or already-waited task: {task_id}"));
                };
                handles.push((task_id.clone(), handle));
            }
            handles
        };

        let waits = handles.into_iter().map(|(task_id, handle)| async move {
            let result = match handle.await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => format!("failed: {error}"),
                Err(error) => format!("join failed: {error}"),
            };
            (task_id, result)
        });
        let results = agentyk::concurrency::join_all(waits).await;
        ToolOutput::text(
            results
                .iter()
                .map(|(task_id, result)| format!("{task_id}: {result}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .with_metadata(json!({
            "state": "succeeded",
            "results": results
        }))
    }
}

#[tokio::main]
async fn main() -> agentyk::Result<()> {
    let child_tasks = Arc::new(ChildTasks::default());
    let task_descriptions = [
        "inspect the event protocol",
        "inspect cancellation behavior",
        "inspect tool concurrency",
        "inspect session replay",
        "inspect capability composition",
    ];
    let task_ids = [
        "subagent_1",
        "subagent_2",
        "subagent_3",
        "subagent_4",
        "subagent_5",
    ];

    let parent = Agent::builder()
        .name("coordinator")
        .system_prompt(
            "Delegate five independent reviews, then wait for and summarize all results.",
        )
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("spawn_subagents", json!({"tasks": task_descriptions})),
            SimTurn::tool_call("wait_for_subagents", json!({"task_ids": task_ids})),
            SimTurn::text("All five subagents completed; their results are in the tool output."),
        ]))
        .tool(SpawnSubagentsTool {
            tasks: child_tasks.clone(),
        })
        .tool(WaitForSubagentsTool { tasks: child_tasks })
        .build()?;

    let mut session = parent.session();
    let result = session
        .run("Delegate these five independent reviews and wait for every result.")
        .await?;
    for message in session
        .messages()
        .iter()
        .filter(|message| message.role == Role::Tool)
    {
        println!("tool result:\n{}\n", message.text());
    }
    println!("{}", result.response);
    Ok(())
}
