//! JSONL event log: durability and replay.

use std::sync::Arc;

use agentyk::{Agent, EventLog, JsonlEventLog, ModelSpec, Result, SimDriver, SimTurn};
use serde_json::json;

#[tokio::test]
async fn jsonl_log_persists_and_resumes_across_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("events.jsonl");

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("noop", json!({})),
            SimTurn::text("done"),
        ]))
        .build()?;

    let session_id = {
        let log = Arc::new(JsonlEventLog::new(&path)?);
        let mut session = agent.session_with_log(log);
        session.run("go").await?;
        session.id()
    };

    // Reopen the file cold: events read back, sequences resume.
    let log = Arc::new(JsonlEventLog::new(&path)?);
    let events = log.read(session_id).await?;
    assert!(events.len() >= 5);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events.last().unwrap().event_type, "turn.completed");

    // Resume the session from the reopened log and keep going.
    let agent_two = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("again")]))
        .build()?;
    let mut resumed = agent_two.resume_session(log.clone(), session_id).await?;
    let turn = resumed.run("more").await?;
    assert_eq!(turn.response, "again");

    // New events continue the same sequence, not a fresh one.
    let all = log.read(session_id).await?;
    let sequences: Vec<u64> = all.iter().map(|e| e.sequence).collect();
    let expected: Vec<u64> = (1..=all.len() as u64).collect();
    assert_eq!(sequences, expected);
    Ok(())
}

#[tokio::test]
async fn two_sessions_share_one_file_with_independent_sequences() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log = Arc::new(JsonlEventLog::new(dir.path().join("shared.jsonl"))?);

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("one"), SimTurn::text("two")]))
        .build()?;

    let mut a = agent.session_with_log(log.clone());
    let mut b = agent.session_with_log(log.clone());
    a.run("x").await?;
    b.run("y").await?;

    let events_a = log.read(a.id()).await?;
    let events_b = log.read(b.id()).await?;
    assert_eq!(events_a[0].sequence, 1);
    assert_eq!(events_b[0].sequence, 1);
    assert_ne!(a.id(), b.id());
    Ok(())
}
