//! Shell-backed hook contract.

use agentyk::{Hook, HookContext, HookEvent, HookOutcome, HookPayload, ShellHook};
use serde_json::json;

#[tokio::test]
async fn shell_hook_receives_payload_and_returns_a_decision() {
    let hook = ShellHook::new(
        "rewrite",
        HookEvent::PreToolUse,
        r#"test "$AGENTYK_HOOK_EVENT" = pre_tool_use &&
           printf '{"decision":"mutate","patch":{"arguments":{"text":"safe"}}}'"#,
    );
    let payload = HookPayload::new(
        HookEvent::PreToolUse,
        "rewrite",
        HookContext::turn(agentyk::SessionId::new(), agentyk::TurnId::new()),
        json!({"tool_name": "echo", "tool_call_id": "call_1", "arguments": {}}),
    );
    assert_eq!(
        hook.run(&payload).await,
        HookOutcome::Mutate {
            patch: json!({"arguments": {"text": "safe"}}),
            reason: None,
        }
    );
}

#[tokio::test]
async fn a_nonzero_empty_decision_blocks_with_stderr() {
    let hook = ShellHook::new(
        "gate",
        HookEvent::UserPromptSubmit,
        "printf 'nope' >&2; exit 3",
    );
    let payload = HookPayload::new(
        HookEvent::UserPromptSubmit,
        "gate",
        HookContext::turn(agentyk::SessionId::new(), agentyk::TurnId::new()),
        json!({"message": "hello"}),
    );
    assert_eq!(
        hook.run(&payload).await,
        HookOutcome::Block {
            reason: "nope".into(),
            user_message: None,
        }
    );
}
