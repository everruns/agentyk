//! Detach a GitHub checks watch, end the foreground turn, then wake the agent
//! with the result.
//!
//! This is the host pattern Yolop uses around Everruns background tasks. The
//! task registry and wake channel are deliberately application-owned here:
//! Agentyk supplies the turns, not a background worker platform.
//!
//! Requires an authenticated GitHub CLI:
//!
//! ```sh
//! cargo run -p agentyk --example github_monitor -- OWNER/REPO PR_NUMBER
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use agentyk::{
    Agent, ModelSpec, SimDriver, SimTurn, Tool, ToolContext, ToolDefinition, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug)]
struct MonitorFinished {
    task_id: String,
    succeeded: bool,
    output: String,
}

struct GithubMonitorTool {
    next_task: AtomicU64,
    finished: mpsc::UnboundedSender<MonitorFinished>,
}

impl GithubMonitorTool {
    fn new(finished: mpsc::UnboundedSender<MonitorFinished>) -> Self {
        Self {
            next_task: AtomicU64::new(1),
            finished,
        }
    }
}

#[async_trait]
impl Tool for GithubMonitorTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "monitor_github_checks",
            "Start `gh pr checks --watch` in the background. Returns immediately; completion wakes the agent.",
            json!({
                "type": "object",
                "properties": {
                    "repository": {
                        "type": "string",
                        "description": "GitHub repository in OWNER/REPO form"
                    },
                    "pull_request": {
                        "type": "integer",
                        "minimum": 1
                    }
                },
                "required": ["repository", "pull_request"],
                "additionalProperties": false
            }),
        )
        .with_metadata(json!({
            "hints": {
                "readonly": true,
                "supports_background": true
            }
        }))
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolOutput {
        let Some(repository) = arguments.get("repository").and_then(|value| value.as_str()) else {
            return ToolOutput::error("repository must be an OWNER/REPO string");
        };
        let Some(pull_request) = arguments
            .get("pull_request")
            .and_then(serde_json::Value::as_u64)
            .filter(|number| *number > 0)
        else {
            return ToolOutput::error("pull_request must be a positive integer");
        };

        let task_id = format!(
            "github_monitor_{}",
            self.next_task.fetch_add(1, Ordering::Relaxed)
        );
        let task_id_for_run = task_id.clone();
        let repository = repository.to_owned();
        let finished = self.finished.clone();

        tokio::spawn(async move {
            // Values are positional parameters, not interpolated into the
            // script, so model-provided repository text cannot become shell.
            let result = Command::new("bash")
                .arg("-lc")
                .arg("gh pr checks \"$1\" --repo \"$2\" --watch")
                .arg("github-monitor")
                .arg(pull_request.to_string())
                .arg(repository)
                .kill_on_drop(true)
                .output()
                .await;

            let completion = match result {
                Ok(output) => {
                    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                    MonitorFinished {
                        task_id: task_id_for_run,
                        succeeded: output.status.success(),
                        output: text,
                    }
                }
                Err(error) => MonitorFinished {
                    task_id: task_id_for_run,
                    succeeded: false,
                    output: error.to_string(),
                },
            };
            let _ = finished.send(completion);
        });

        ToolOutput::text(format!(
            "Started {task_id}. End this turn; the host will wake you when GitHub checks finish."
        ))
        .with_metadata(json!({
            "task_id": task_id,
            "state": "running",
            "kind": "background_tool"
        }))
    }
}

#[tokio::main]
async fn main() -> agentyk::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(repository) = args.next() else {
        eprintln!("usage: cargo run -p agentyk --example github_monitor -- OWNER/REPO PR_NUMBER");
        return Ok(());
    };
    let Some(pull_request) = args.next().and_then(|value| value.parse::<u64>().ok()) else {
        eprintln!("PR_NUMBER must be a positive integer");
        return Ok(());
    };

    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
    let agent = Agent::builder()
        .name("github-monitor")
        .system_prompt(
            "Start one detached GitHub checks watch, end the turn, and review its completion wake.",
        )
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call(
                "monitor_github_checks",
                json!({
                    "repository": repository,
                    "pull_request": pull_request
                }),
            ),
            SimTurn::text("The GitHub monitor is running in the background."),
            SimTurn::text("I reviewed the completed GitHub checks."),
        ]))
        .tool(GithubMonitorTool::new(finished_tx))
        .build()?;

    let mut session = agent.session();
    let started = session
        .run("Monitor this pull request until its checks finish.")
        .await?;
    println!("agent: {}", started.response);

    let Some(completion) = finished_rx.recv().await else {
        return Err(agentyk::Error::Other(
            "GitHub monitor ended without reporting completion".into(),
        ));
    };
    let status = if completion.succeeded {
        "succeeded"
    } else {
        "failed"
    };
    let wake = format!(
        "[automatic] Background task {} {status}.\n\n{}",
        completion.task_id,
        completion.output.trim()
    );
    let reviewed = session.run(wake).await?;
    println!("agent: {}", reviewed.response);
    Ok(())
}
