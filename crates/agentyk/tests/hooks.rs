//! Everruns-parity user hook lifecycle, end to end.

use std::sync::{Arc, Mutex};

use agentyk::{
    Agent, FnTool, Hook, HookContext, HookErrorPolicy, HookEvent, HookMatcher, HookOutcome,
    HookPayload, Message, ModelSpec, Provider, Result, SimDriver, SimTurn, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;

struct FixedHook {
    id: &'static str,
    event: HookEvent,
    outcome: HookOutcome,
    matcher: Option<HookMatcher>,
    seen: Arc<Mutex<Vec<HookPayload>>>,
    on_error: HookErrorPolicy,
}

#[async_trait]
impl Hook for FixedHook {
    fn id(&self) -> &str {
        self.id
    }

    fn event(&self) -> HookEvent {
        self.event
    }

    fn matcher(&self) -> Option<&HookMatcher> {
        self.matcher.as_ref()
    }

    fn on_error(&self) -> HookErrorPolicy {
        self.on_error
    }

    async fn run(&self, payload: &HookPayload) -> HookOutcome {
        self.seen.lock().unwrap().push(payload.clone());
        self.outcome.clone()
    }
}

fn hook(
    id: &'static str,
    event: HookEvent,
    outcome: HookOutcome,
    seen: Arc<Mutex<Vec<HookPayload>>>,
) -> FixedHook {
    FixedHook {
        id,
        event,
        outcome,
        matcher: None,
        seen,
        on_error: HookErrorPolicy::Warn,
    }
}

#[tokio::test]
async fn prompt_tool_and_turn_hooks_mutate_in_order() -> Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed_argument = Arc::new(Mutex::new(String::new()));
    let tool_observation = observed_argument.clone();
    let driver = SimDriver::new([
        SimTurn::tool_call("echo", json!({"text": "unsafe"})),
        SimTurn::text("done"),
    ]);

    let pre = FixedHook {
        matcher: Some(HookMatcher {
            tool_name: Some("echo".into()),
            ..HookMatcher::default()
        }),
        ..hook(
            "pre",
            HookEvent::PreToolUse,
            HookOutcome::Mutate {
                patch: json!({"arguments": {"text": "safe"}}),
                reason: Some("redacted".into()),
            },
            seen.clone(),
        )
    };
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(driver))
        .tool(FnTool::new(
            "echo",
            "Echo text",
            json!({"type": "object"}),
            move |arguments| {
                let tool_observation = tool_observation.clone();
                async move {
                    *tool_observation.lock().unwrap() =
                        arguments["text"].as_str().unwrap().to_string();
                    ToolOutput::text("raw")
                }
            },
        ))
        .hook(hook(
            "prompt",
            HookEvent::UserPromptSubmit,
            HookOutcome::Mutate {
                patch: json!({"message": "rewritten"}),
                reason: None,
            },
            seen.clone(),
        ))
        .hook(pre)
        .hook(hook(
            "post",
            HookEvent::PostToolUse,
            HookOutcome::Mutate {
                patch: json!({"result": "filtered", "additional_context": "checked"}),
                reason: None,
            },
            seen.clone(),
        ))
        .hook(hook(
            "end",
            HookEvent::TurnEnd,
            HookOutcome::Allow,
            seen.clone(),
        ))
        .build()?;

    let mut session = agent.session();
    let result = session.run("original").await?;
    assert_eq!(result.response, "done");
    assert_eq!(*observed_argument.lock().unwrap(), "safe");
    assert_eq!(session.messages()[0], Message::user("rewritten"));

    let tool_result = session.events().await?.into_iter().find_map(|event| {
        if let agentyk::EventData::ToolCompleted { output, .. } = event.data {
            Some(output)
        } else {
            None
        }
    });
    assert_eq!(tool_result.as_deref(), Some("filtered\nchecked"));

    let events: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|payload| payload.event)
        .collect();
    assert_eq!(
        events,
        [
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::TurnEnd,
        ]
    );
    Ok(())
}

#[tokio::test]
async fn blockable_and_advisory_semantics_are_distinct() -> Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "unreachable",
        )])))
        .hook(hook(
            "start",
            HookEvent::SessionStart,
            HookOutcome::Block {
                reason: "ignored".into(),
                user_message: None,
            },
            seen.clone(),
        ))
        .hook(hook(
            "prompt",
            HookEvent::UserPromptSubmit,
            HookOutcome::Block {
                reason: "policy".into(),
                user_message: Some("not allowed".into()),
            },
            seen.clone(),
        ))
        .hook(hook(
            "end",
            HookEvent::SessionEnd,
            HookOutcome::Allow,
            seen.clone(),
        ))
        .build()?;

    let mut session = agent.session();
    let error = session.run("blocked").await.unwrap_err();
    assert!(matches!(
        error,
        agentyk::Error::HookBlocked {
            user_message: Some(_),
            ..
        }
    ));
    session.close().await?;
    session.close().await?;

    let events: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|payload| payload.event)
        .collect();
    assert_eq!(
        events,
        [
            HookEvent::SessionStart,
            HookEvent::UserPromptSubmit,
            HookEvent::SessionEnd,
        ]
    );
    assert!(session.is_closed());
    Ok(())
}

#[tokio::test]
async fn warn_on_error_records_a_hook_warning_without_blocking() -> Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("ok")])))
        .hook(hook(
            "broken-start",
            HookEvent::SessionStart,
            HookOutcome::Error {
                message: "executor unavailable".into(),
            },
            seen,
        ))
        .build()?;

    let mut session = agent.session();
    assert_eq!(session.run("hello").await?.response, "ok");
    let warning = session
        .events()
        .await?
        .into_iter()
        .find(|event| event.event_type == "hook.warning")
        .expect("warn-on-error is durable");
    assert_eq!(warning.turn_id, None);
    Ok(())
}

#[test]
fn matcher_on_a_non_tool_event_is_a_build_error() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let invalid = FixedHook {
        matcher: Some(HookMatcher {
            tool_name: Some("echo".into()),
            ..HookMatcher::default()
        }),
        ..hook("invalid", HookEvent::TurnEnd, HookOutcome::Allow, seen)
    };
    let result = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new(
            std::iter::empty::<SimTurn>(),
        )))
        .hook(invalid)
        .build();
    assert!(matches!(result, Err(agentyk::Error::InvalidAgent(_))));
}

#[test]
fn hook_context_serializes_stable_ids() {
    let context = HookContext::session(agentyk::SessionId::new());
    assert!(serde_json::to_value(context).unwrap()["session_id"].is_string());
    let payload = HookPayload::new(HookEvent::SessionStart, "start", context, json!({}));
    let value = serde_json::to_value(payload).unwrap();
    assert!(value["session_id"].is_string());
    assert!(value.get("context").is_none());
    assert!(value["ts"].is_string());
}
