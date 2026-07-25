//! The transcript is what the operator actually sees, so it is tested against
//! a real turn — same executor, same event ordering, same middleware — driven by
//! the scripted `SimDriver`. No network, no API key, no terminal.

use std::sync::Arc;

use agentyk::{
    Agent, CancellationToken, EventListener, FnTool, InMemoryEventLog, ModelSpec, SimDriver,
    SimTurn, ToolCallDecision, ToolInvocation, ToolOutput, TurnMiddleware,
};
use async_trait::async_trait;
use codenko::agent::{AppEvent, AppEventSender, Prompt, TurnOutcome, spawn_agent_task};
use codenko::transcript::{Entry, ToolState, Transcript, summarize_arguments};
use serde_json::json;
use tokio::sync::mpsc;

/// Answers every gated call the same way — the operator at the keyboard,
/// scripted.
struct FixedApproval(bool);

#[async_trait]
impl TurnMiddleware for FixedApproval {
    async fn before_tool(&self, _invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        match self.0 {
            true => ToolCallDecision::Proceed,
            false => ToolCallDecision::Deny {
                reason: "the operator declined this call".into(),
            },
        }
    }
}

/// Forwards events onto the UI channel, exactly as codenko's listener does.
struct Forward(AppEventSender);

#[async_trait]
impl EventListener for Forward {
    async fn on_event(&self, event: &agentyk::Event) {
        let _ = self.0.send(AppEvent::Agent(Box::new(event.clone())));
    }
}

fn echo_tool() -> FnTool {
    FnTool::new(
        "run_command",
        "Run a command.",
        json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        |arguments| async move {
            ToolOutput::text(format!(
                "ran: {}",
                arguments["command"].as_str().unwrap_or_default()
            ))
        },
    )
}

/// Run one scripted turn end to end and fold its events, the way the UI's
/// update loop does. Returns what the operator would be looking at.
async fn run_turn(
    turns: Vec<SimTurn>,
    approve: bool,
    max_iterations: usize,
    input: &str,
) -> (Transcript, TurnOutcome) {
    let (events, mut inbox) = mpsc::unbounded_channel();
    let agent = Agent::builder()
        .name("codenko-test")
        .system_prompt("scripted")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new(turns))
        .tool(echo_tool())
        .middleware(FixedApproval(approve))
        .listener_arc(Arc::new(Forward(events.clone())))
        .max_iterations(max_iterations)
        .build()
        .expect("agent builds");

    let prompts = spawn_agent_task(agent, Arc::new(InMemoryEventLog::new()), events);
    let cancel = CancellationToken::new();
    prompts
        .send(Prompt {
            text: input.to_string(),
            cancel,
        })
        .expect("agent task accepts the prompt");

    let mut transcript = Transcript::new();
    loop {
        match inbox.recv().await.expect("the agent task stays alive") {
            AppEvent::Agent(event) => transcript.apply(&event),
            AppEvent::TurnEnded(outcome) => return (transcript, outcome),
            AppEvent::Approval { .. } => unreachable!("the test hook answers directly"),
        }
    }
}

#[tokio::test]
async fn a_tool_calling_turn_becomes_user_tool_assistant() {
    let (transcript, outcome) = run_turn(
        vec![
            SimTurn::tool_call("run_command", json!({"command": "ls"})),
            SimTurn::text("main.rs and ui.rs."),
        ],
        true,
        8,
        "what is in here?",
    )
    .await;

    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            iterations: 2,
            tool_calls: 1
        }
    );

    let entries = transcript.entries();
    assert_eq!(entries[0], Entry::User("what is in here?".into()));
    match &entries[1] {
        Entry::Tool {
            name,
            summary,
            output,
            state,
            ..
        } => {
            assert_eq!(name, "run_command");
            // A single argument is summarized bare, not as `command=ls`.
            assert_eq!(summary, "ls");
            assert_eq!(output, "ran: ls");
            assert_eq!(*state, ToolState::Ok);
        }
        other => panic!("expected a tool entry, got {other:?}"),
    }
    assert_eq!(entries[2], Entry::Assistant("main.rs and ui.rs.".into()));
    // No empty assistant bubble for the tool-call-only reason step.
    assert_eq!(entries.len(), 3, "{entries:?}");
}

#[tokio::test]
async fn a_declined_call_is_shown_as_denied_with_its_reason() {
    let (transcript, _) = run_turn(
        vec![
            SimTurn::tool_call("run_command", json!({"command": "rm -rf /"})),
            SimTurn::text("Understood, leaving it alone."),
        ],
        false,
        8,
        "clean up",
    )
    .await;

    let denied = transcript
        .entries()
        .iter()
        .find_map(|entry| match entry {
            Entry::Tool { state, output, .. } if *state == ToolState::Denied => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("a denied tool entry");
    assert!(denied.contains("declined"), "{denied}");
}

#[tokio::test]
async fn streaming_deltas_grow_one_assistant_entry() {
    // `SimDriver` reports its whole response through the trait's default
    // `complete_streaming`, so one entry is opened by the delta and then
    // settled by the completed message.
    let (transcript, _) = run_turn(vec![SimTurn::text("hello there")], true, 8, "hi").await;

    let assistants: Vec<&Entry> = transcript
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Assistant(_)))
        .collect();
    assert_eq!(assistants.len(), 1, "{assistants:?}");
    assert_eq!(assistants[0], &Entry::Assistant("hello there".into()));
}

#[tokio::test]
async fn a_failed_turn_leaves_a_notice_in_the_transcript() {
    // One iteration is not enough to answer after a tool call, so the turn
    // ends on the iteration ceiling.
    let (transcript, outcome) = run_turn(
        vec![SimTurn::tool_call("run_command", json!({"command": "ls"}))],
        true,
        1,
        "go",
    )
    .await;

    assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
    assert!(
        transcript
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Notice(text) if text.contains("turn failed"))),
        "{:?}",
        transcript.entries()
    );
}

#[tokio::test]
async fn a_cancelled_turn_reports_itself_as_interrupted() {
    let (events, mut inbox) = mpsc::unbounded_channel();
    let agent = Agent::builder()
        .name("codenko-test")
        .system_prompt("scripted")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("never sent")]))
        .listener_arc(Arc::new(Forward(events.clone())))
        .build()
        .expect("agent builds");
    let prompts = spawn_agent_task(agent, Arc::new(InMemoryEventLog::new()), events);

    let cancel = CancellationToken::new();
    // Cancelled before the turn starts: the executor stops at its first check
    // point, which is the same path esc takes mid-turn.
    cancel.cancel();
    prompts
        .send(Prompt {
            text: "go".into(),
            cancel,
        })
        .expect("agent task accepts the prompt");

    let mut transcript = Transcript::new();
    let outcome = loop {
        match inbox.recv().await.expect("the agent task stays alive") {
            AppEvent::Agent(event) => transcript.apply(&event),
            AppEvent::TurnEnded(outcome) => break outcome,
            AppEvent::Approval { .. } => unreachable!("no tools are attached"),
        }
    };

    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(
        transcript
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Notice(text) if text == "interrupted")),
        "{:?}",
        transcript.entries()
    );
}

#[test]
fn multi_argument_calls_are_summarized_as_pairs() {
    let summary = summarize_arguments(&json!({"path": "src/main.rs", "content": "fn main() {}"}));
    assert!(summary.contains("path=src/main.rs"), "{summary}");
    assert!(summary.contains("content=fn main() {}"), "{summary}");
}

#[test]
fn a_long_value_never_hides_the_others() {
    // `write_file` on a large file: the path still has to be readable.
    let summary = summarize_arguments(&json!({
        "path": "fizz.py",
        "content": "def fizzbuzz(n):\n".repeat(40),
    }));
    assert!(summary.contains("path=fizz.py"), "{summary}");
    assert!(summary.contains('…'), "{summary}");
    assert!(summary.chars().count() < 80, "{summary}");
}

#[test]
fn summaries_are_single_line_and_bounded() {
    assert_eq!(
        summarize_arguments(&json!({"command": "a\n  b\n  c"})),
        "a b c"
    );
    let long = summarize_arguments(&json!({"command": "x".repeat(400)}));
    assert_eq!(long.chars().count(), 120);
    assert!(long.ends_with('…'));
}
