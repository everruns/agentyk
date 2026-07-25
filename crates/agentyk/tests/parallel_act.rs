//! `TurnState::pending_tool_actions` — the parallel-capable batch API.
//!
//! `InProcessExecutor` stays sequential (drains `next_action` one call at a
//! time — proven elsewhere), but the state machine itself supports an
//! executor that fans a whole batch out concurrently and completes calls in
//! any order. This test drives the machine that way directly, standing in
//! for what a parallel executor would do.

use agentyk::{
    ChatResponse, Message, Result, SessionId, ToolCall, ToolContext, ToolOutput, TurnAction,
    TurnState, Usage,
};
use serde_json::json;

#[test]
fn a_batch_can_be_dispatched_concurrently_and_completed_out_of_order() -> Result<()> {
    let session_id = SessionId::new();
    let (mut state, _) = TurnState::start(session_id, 8, &Message::user("go"));

    // Simulate a reason step's response requesting three concurrent calls.
    let response = ChatResponse::new(
        Message::assistant_with_calls(
            "",
            vec![
                ToolCall {
                    id: "call_a".into(),
                    name: "echo".into(),
                    arguments: json!({"n": 1}),
                },
                ToolCall {
                    id: "call_b".into(),
                    name: "echo".into(),
                    arguments: json!({"n": 2}),
                },
                ToolCall {
                    id: "call_c".into(),
                    name: "echo".into(),
                    arguments: json!({"n": 3}),
                },
            ],
        ),
        Usage::default(),
    );
    state.on_reason_completed(&response);

    // A parallel executor fans out the whole ready batch at once...
    let actions = state.pending_tool_actions();
    assert_eq!(actions.len(), 3);
    let calls: Vec<ToolCall> = actions
        .into_iter()
        .map(|a| {
            let TurnAction::ExecuteTool { call } = a else {
                unreachable!()
            };
            call
        })
        .collect();

    // ...marks every one started up front (a real executor would do this
    // right before dispatching each concurrently)...
    for call in &calls {
        let effects = state.on_tool_started(&call.id);
        assert_eq!(effects.len(), 1);
    }
    assert!(
        state.pending_tool_actions().is_empty(),
        "every call in the batch has been started"
    );

    // ...and completes them in an order that has nothing to do with how
    // they were dispatched (call_c first, call_a last) — proving
    // completion order doesn't matter to the state machine.
    let _context = ToolContext::new(session_id, state.turn_id);
    for call in [&calls[2], &calls[1], &calls[0]] {
        let output = ToolOutput::text(format!("echoed {}", call.id));
        let effects = state.on_tool_completed(&call.id, &output);
        assert_eq!(effects.len(), 1);
    }

    // The batch is fully drained: back to PendingReason, all three counted.
    assert_eq!(state.next_action(), TurnAction::Reason);
    assert_eq!(state.tool_calls_executed, 3);

    // Re-completing an already-resolved call is a no-op (idempotent, like
    // on_tool_started) — safe if a durable host re-delivers a result.
    assert!(
        state
            .on_tool_completed(&calls[0].id, &ToolOutput::text("dup"))
            .is_empty()
    );
    Ok(())
}
