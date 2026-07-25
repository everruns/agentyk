//! The agent half of codenko: how the agent is composed, and the task that
//! owns its session.
//!
//! The UI never touches the [`agentyk::Session`] — a session is `&mut self` for a
//! whole turn, so it lives alone in [`spawn_agent_task`] and communicates
//! with the UI over two channels: prompts in, [`AppEvent`]s out. Everything
//! the UI shows is derived from the agentyk event stream, delivered by an
//! [`EventListener`] on the same channel.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agentyk::{
    Agent, AnthropicDriver, CancellationToken, Event, EventListener, EventLog,
    FileSystemCapability, FnTool, ModelSpec, RealDiskFileSystem, ToolCall, ToolCallDecision,
    ToolInvocation, ToolOutput, TurnMiddleware,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// Tools that mutate the workspace and therefore ask before running. Read-only
/// tools (`read_file`, `list_directory`) are never gated — a coding agent that
/// asks permission to look at a file is unusable.
pub const GATED_TOOLS: &[&str] = &["run_command", "write_file", "delete_file"];

/// Ceiling on how much of a command's output goes back to the model. Without
/// it, one `cat` of a large file fills the context window.
const MAX_TOOL_OUTPUT: usize = 8_000;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

const SYSTEM_PROMPT: &str = "\
You are codenko, a terse coding agent working in a single workspace directory.

Work by acting, not by narrating: read what you need, make the change, verify
it. Prefer `list_directory` and `read_file` over shell equivalents — they are
cheaper and never need approval. Use `run_command` for everything else (git,
builds, tests, search).

`run_command`, `write_file`, and `delete_file` are gated: the operator sees the
call and approves or declines it. A declined call is a decision, not an error —
say what you would have done and stop, do not try to route around it.

Keep replies to a few lines. Report what you found or changed, not how you
looked. When a command fails, show the part of the output that explains why.";

/// One prompt handed to the agent task, with the token that stops it.
pub struct Prompt {
    pub text: String,
    pub cancel: CancellationToken,
}

/// How a turn ended. `Cancelled` is a first-class outcome, not a failure —
/// the operator pressing esc is the expected way to stop a long turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Completed {
        iterations: usize,
        tool_calls: usize,
    },
    Cancelled,
    Failed(String),
}

/// Everything the UI reacts to, from both the event stream and the hooks.
pub enum AppEvent {
    /// An agentyk protocol event — the transcript is built entirely from these.
    Agent(Box<Event>),
    TurnEnded(TurnOutcome),
    /// A gated tool is waiting on the operator. Dropping `reply` denies it.
    Approval {
        call: ToolCall,
        reply: oneshot::Sender<bool>,
    },
}

pub type AppEventSender = mpsc::UnboundedSender<AppEvent>;

/// Forwards every event — durable and streaming-delta alike — to the UI.
struct ChannelListener(AppEventSender);

#[async_trait]
impl EventListener for ChannelListener {
    async fn on_event(&self, event: &Event) {
        // A closed channel means the UI is gone; the run is ending anyway.
        let _ = self.0.send(AppEvent::Agent(Box::new(event.clone())));
    }

    fn name(&self) -> &'static str {
        "codenko-ui"
    }
}

/// Turns [`GATED_TOOLS`] calls into a UI prompt and waits for the answer.
/// This is the whole of codenko's approval flow: middleware blocks the turn
/// while it awaits, so no state machine is needed on either side.
struct ApprovalMiddleware(AppEventSender);

#[async_trait]
impl TurnMiddleware for ApprovalMiddleware {
    fn name(&self) -> &str {
        "codenko-approval"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if !GATED_TOOLS.contains(&invocation.call.name.as_str()) {
            return ToolCallDecision::Proceed;
        }
        let (reply, answer) = oneshot::channel();
        let request = AppEvent::Approval {
            call: invocation.call.clone(),
            reply,
        };
        if self.0.send(request).is_err() {
            return deny("the operator is no longer available");
        }
        match answer.await {
            Ok(true) => ToolCallDecision::Proceed,
            Ok(false) => deny("the operator declined this call"),
            // The UI dropped the sender (quit mid-prompt): fail closed.
            Err(_) => deny("the approval prompt was dismissed"),
        }
    }
}

fn deny(reason: &str) -> ToolCallDecision {
    ToolCallDecision::Deny {
        reason: reason.to_string(),
    }
}

/// `run_command` — a bash shell rooted at the workspace.
///
/// Deliberately one broad tool rather than a tool per operation: breadth is
/// what makes a small agent useful, and the approval hook is what makes it
/// safe. Output is truncated and the command is killed after
/// [`COMMAND_TIMEOUT`] so neither the context window nor the UI can be taken
/// hostage by one call.
fn run_command_tool(workspace: PathBuf) -> FnTool {
    FnTool::new(
        "run_command",
        "Run a bash command in the workspace directory. Use for git, builds, \
         tests, and search. Requires operator approval.",
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command line to run."
                }
            },
            "required": ["command"]
        }),
        move |arguments| {
            let workspace = workspace.clone();
            async move {
                let Some(command) = arguments["command"].as_str() else {
                    return ToolOutput::error("`command` is required and must be a string");
                };
                run_command(&workspace, command).await
            }
        },
    )
}

async fn run_command(workspace: &Path, command: &str) -> ToolOutput {
    let spawned = tokio::process::Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();

    let output = match tokio::time::timeout(COMMAND_TIMEOUT, spawned).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return ToolOutput::error(format!("failed to run command: {error}")),
        Err(_) => {
            return ToolOutput::error(format!(
                "command timed out after {}s",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
    };

    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if body.trim().is_empty() {
        body = "(no output)".to_string();
    }
    let body = truncate(body, MAX_TOOL_OUTPUT);

    // A non-zero exit is reported as a tool error so the model sees the
    // failure rather than inferring it from the text.
    match output.status.success() {
        true => ToolOutput::text(body),
        false => ToolOutput::error(format!(
            "exit status {}\n{body}",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string())
        )),
    }
}

fn truncate(mut text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(index, _)| index);
    text.truncate(cut);
    text.push_str("\n… output truncated");
    text
}

/// Compose the agent: prompt, model, workspace tools, approval gate.
///
/// Everything is by value — there is nothing to register and no id to thread.
/// Swapping the approval policy or the workspace means passing a different
/// object here, not reconfiguring the agent afterwards.
pub fn build_agent(
    workspace: &Path,
    model: ModelSpec,
    events: AppEventSender,
) -> agentyk::Result<Agent> {
    let files = RealDiskFileSystem::new(workspace)?;
    Agent::builder()
        .name("codenko")
        .system_prompt(SYSTEM_PROMPT)
        .model(model)
        // Room for a long tool-calling turn; the default 8k truncates edits.
        .driver(AnthropicDriver::new().max_tokens(16_000))
        .capability(FileSystemCapability::new(files))
        .tool(run_command_tool(workspace.to_path_buf()))
        .middleware(ApprovalMiddleware(events.clone()))
        .listener(ChannelListener(events))
        .max_iterations(24)
        .build()
}

/// Own the session in one task: prompts in, outcomes out.
pub fn spawn_agent_task(
    agent: Agent,
    log: Arc<dyn EventLog>,
    events: AppEventSender,
) -> mpsc::UnboundedSender<Prompt> {
    let (prompts, mut inbox) = mpsc::unbounded_channel::<Prompt>();
    tokio::spawn(async move {
        let mut session = agent.session_with_log(log);
        while let Some(prompt) = inbox.recv().await {
            let outcome = match session.run_cancellable(prompt.text, prompt.cancel).await {
                Ok(turn) => TurnOutcome::Completed {
                    iterations: turn.iterations,
                    tool_calls: turn.tool_calls,
                },
                Err(agentyk::Error::Cancelled) => TurnOutcome::Cancelled,
                Err(error) => TurnOutcome::Failed(error.to_string()),
            };
            if events.send(AppEvent::TurnEnded(outcome)).is_err() {
                break;
            }
        }
    });
    prompts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_output_combines_streams_and_reports_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let ok = run_command(workspace.path(), "echo out; echo err >&2").await;
        assert!(!ok.is_error, "{ok:?}");
        assert!(ok.content.contains("out") && ok.content.contains("err"));

        let failed = run_command(workspace.path(), "exit 3").await;
        assert!(failed.is_error);
        assert!(failed.content.contains("exit status 3"));
    }

    #[tokio::test]
    async fn commands_run_in_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("marker.txt"), "hi").unwrap();
        let output = run_command(workspace.path(), "ls").await;
        assert!(output.content.contains("marker.txt"), "{output:?}");
    }

    #[test]
    fn truncation_appends_a_marker() {
        let text = truncate("x".repeat(20), 8);
        assert!(text.starts_with(&"x".repeat(8)));
        assert!(text.contains("truncated"));
    }
}
