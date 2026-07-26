//! JSONL event log: durability and replay.

use std::sync::Arc;

use agentyk::{
    Agent, Error, EventData, EventLog, EventRange, EventRequest, ExpectedVersion, JsonlEventLog,
    ModelSpec, Result, SessionId, SimDriver, SimTurn,
};
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
    assert_eq!(events[0].sequence, Some(1));
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
    let sequences: Vec<Option<u64>> = all.iter().map(|e| e.sequence).collect();
    let expected: Vec<Option<u64>> = (1..=all.len() as u64).map(Some).collect();
    assert_eq!(sequences, expected);
    Ok(())
}

/// A log written by a newer agentyk must stay readable — and *resumable* — by
/// an older one. Before `Event` grew a tolerant `Deserialize`, one unknown
/// `kind` failed the whole `read()`, taking every other event in the session
/// with it and making the session unresumable.
#[tokio::test]
async fn a_log_containing_a_future_event_kind_still_reads_and_resumes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("events.jsonl");

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("first")]))
        .build()?;

    let session_id = {
        let log = Arc::new(JsonlEventLog::new(&path)?);
        let mut session = agent.session_with_log(log);
        session.run("go").await?;
        session.id()
    };

    // Splice in an event this build has no variant for, as a future version
    // would have written it.
    let known = std::fs::read_to_string(&path)?;
    let last: serde_json::Value = serde_json::from_str(known.lines().last().unwrap())?;
    let future_event = json!({
        "id": "event_0199c4a70000700080008000800080ab",
        "type": "turn.compacted",
        "ts": last["ts"],
        "session_id": last["session_id"],
        "sequence": last["sequence"].as_u64().unwrap() + 1,
        "data": {"kind": "turn_compacted", "removed_messages": 4},
    });
    std::fs::write(&path, format!("{known}{future_event}\n"))?;

    let log = Arc::new(JsonlEventLog::new(&path)?);
    let events = log.read(session_id).await?;

    // Every known event survives, and the unknown one is preserved rather
    // than dropped — a host can still forward what it can't interpret.
    assert!(events.iter().any(|e| e.event_type == "turn.completed"));
    let unknown = events
        .iter()
        .find(|e| e.event_type == "turn.compacted")
        .expect("the future event should be readable");
    assert!(matches!(
        &unknown.data,
        agentyk::EventData::Custom { payload, .. } if payload["removed_messages"] == 4
    ));

    // And the session still resumes: history replays past the unknown event.
    let agent_two = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("second")]))
        .build()?;
    let mut resumed = agent_two.resume_session(log, session_id).await?;
    assert_eq!(resumed.run("more").await?.response, "second");
    assert!(resumed.messages().iter().any(|m| m.text() == "first"));
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
    assert_eq!(events_a[0].sequence, Some(1));
    assert_eq!(events_b[0].sequence, Some(1));
    assert_ne!(a.id(), b.id());
    Ok(())
}

#[tokio::test]
async fn jsonl_batches_are_version_checked_and_incrementally_readable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log = JsonlEventLog::new(dir.path().join("batch.jsonl"))?;
    let session_id = SessionId::new();
    let requests = vec![
        EventRequest::new(session_id, EventData::TurnStarted { max_iterations: 4 }),
        EventRequest::new(session_id, EventData::TurnCancelled),
    ];

    let appended = log
        .append_batch(session_id, ExpectedVersion::Exact(0), requests)
        .await?;
    assert_eq!(appended[0].sequence, Some(1));
    assert_eq!(appended[1].sequence, Some(2));

    let conflict = log
        .append_batch(
            session_id,
            ExpectedVersion::Exact(0),
            vec![EventRequest::new(session_id, EventData::TurnCancelled)],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        conflict,
        Error::EventConflict {
            expected: 0,
            actual: 2
        }
    ));
    assert_eq!(log.read_after(session_id, Some(1)).await?.len(), 1);
    let page = log.read_page(EventRange::new(session_id).limit(1)).await?;
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.head.sequence, 2);
    assert_eq!(page.next.expect("second page").sequence, 1);
    Ok(())
}
