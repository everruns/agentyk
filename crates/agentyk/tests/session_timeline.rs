//! Historical inspection, paged reads, forks, and disposable snapshots.

use std::sync::Arc;

use agentyk::{
    Agent, EventData, EventRange, EventStore, InMemoryEventLog, InMemorySnapshotStore,
    JsonlEventLog, ModelSpec, ProjectionSnapshot, Provider, Result, SessionPoint, SimDriver,
    SimTurn, SnapshotStore,
};
use serde_json::json;

#[tokio::test]
async fn a_completed_session_point_can_be_inspected_and_forked() -> Result<()> {
    let log = Arc::new(InMemoryEventLog::new());
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([
            SimTurn::text("shared answer"),
            SimTurn::text("fork answer"),
            SimTurn::text("original answer"),
        ])))
        .build()?;

    let mut original = agent.session_with_log(log.clone());
    original.run("shared question").await?;
    let branch_point = original.point().await?;

    let view = original.inspect(branch_point).await?;
    assert_eq!(view.point(), branch_point);
    assert_eq!(view.messages(), original.messages());

    let mut fork = original.fork(branch_point).await?;
    assert_ne!(fork.id(), original.id());
    assert_eq!(fork.messages(), original.messages());
    let fork_events = fork.events().await?;
    assert!(fork_events.iter().any(|event| {
        matches!(
            event.data,
            EventData::SessionForked {
                parent_session_id,
                parent_sequence,
            } if parent_session_id == original.id() && parent_sequence == branch_point.sequence
        )
    }));

    fork.run("take the alternative").await?;
    original.run("stay on the original").await?;
    assert_eq!(fork.messages().last().unwrap().text(), "fork answer");
    assert_eq!(
        original.messages().last().unwrap().text(),
        "original answer"
    );
    assert!(
        !original
            .events()
            .await?
            .iter()
            .any(|event| event.event_type == "session.forked")
    );
    Ok(())
}

#[tokio::test]
async fn a_mid_turn_event_is_inspectable_but_not_forkable() -> Result<()> {
    let log = Arc::new(InMemoryEventLog::new());
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("done")])))
        .build()?;
    let mut session = agent.session_with_log(log);
    session.run("go").await?;

    let turn_started = session
        .events()
        .await?
        .into_iter()
        .find(|event| matches!(event.data, EventData::TurnStarted { .. }))
        .expect("turn.started")
        .sequence
        .expect("durable");
    let point = SessionPoint::new(session.id(), turn_started);
    assert!(session.inspect(point).await?.messages().is_empty());
    let error = match session.fork(point).await {
        Ok(_) => panic!("mid-turn fork must be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("completed-turn boundary"));
    Ok(())
}

#[tokio::test]
async fn event_pages_are_bounded_and_report_the_observed_head() -> Result<()> {
    let log = InMemoryEventLog::new();
    let session_id = agentyk::SessionId::new();
    let requests = (0..5)
        .map(|index| {
            agentyk::EventRequest::new(
                session_id,
                EventData::custom("test.item", json!({ "index": index })),
            )
        })
        .collect();
    log.append_batch(session_id, agentyk::ExpectedVersion::Exact(0), requests)
        .await?;

    let first = log.read_page(EventRange::new(session_id).limit(2)).await?;
    assert_eq!(first.events.len(), 2);
    assert_eq!(first.head.sequence, 5);
    let second = log
        .read_page(
            EventRange::new(session_id)
                .after(first.next.expect("next page").sequence)
                .limit(2),
        )
        .await?;
    assert_eq!(second.events.len(), 2);
    let third = log
        .read_page(
            EventRange::new(session_id)
                .after(second.next.expect("next page").sequence)
                .limit(2),
        )
        .await?;
    assert_eq!(third.events.len(), 1);
    assert!(third.next.is_none());
    Ok(())
}

#[tokio::test]
async fn convenience_reads_cross_page_boundaries_without_losing_events() -> Result<()> {
    let log = InMemoryEventLog::new();
    let session_id = agentyk::SessionId::new();
    let requests = (0..513)
        .map(|index| {
            agentyk::EventRequest::new(
                session_id,
                EventData::custom("test.item", json!({ "index": index })),
            )
        })
        .collect();
    log.append_batch(session_id, agentyk::ExpectedVersion::Exact(0), requests)
        .await?;

    let events = log.read(session_id).await?;
    assert_eq!(events.len(), 513);
    assert_eq!(events.last().unwrap().sequence, Some(513));
    Ok(())
}

#[tokio::test]
async fn snapshots_load_by_projection_and_historical_upper_bound() -> Result<()> {
    let store = InMemorySnapshotStore::new();
    let session_id = agentyk::SessionId::new();
    for sequence in [5, 10] {
        store
            .save(ProjectionSnapshot {
                point: SessionPoint::new(session_id, sequence),
                projection: "history".into(),
                schema_version: 1,
                payload: json!({ "through": sequence }),
            })
            .await?;
    }

    let historical = store
        .load_latest(session_id, "history", 1, Some(7))
        .await?
        .expect("snapshot");
    assert_eq!(historical.point.sequence, 5);
    let latest = store
        .load_latest(session_id, "history", 1, None)
        .await?
        .expect("snapshot");
    assert_eq!(latest.point.sequence, 10);
    assert!(
        store
            .load_latest(session_id, "turn_state", 1, None)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn a_jsonl_fork_retains_lineage_and_resumes_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("forks.jsonl");
    let parent_agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text("parent")])))
        .build()?;
    let child_id = {
        let log = Arc::new(JsonlEventLog::new(&path)?);
        let mut parent = parent_agent.session_with_log(log);
        parent.run("begin").await?;
        let child = parent.fork(parent.point().await?).await?;
        child.id()
    };

    let log = Arc::new(JsonlEventLog::new(&path)?);
    let child_agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "continued",
        )])))
        .build()?;
    let mut child = child_agent.resume_session(log, child_id).await?;
    assert_eq!(child.messages().last().unwrap().text(), "parent");
    child.run("continue child").await?;
    assert_eq!(child.messages().last().unwrap().text(), "continued");
    assert!(
        child
            .events()
            .await?
            .iter()
            .any(|event| event.event_type == "session.forked")
    );
    Ok(())
}

#[tokio::test]
async fn an_incomplete_head_turn_resumes_without_new_user_input() -> Result<()> {
    let log = Arc::new(InMemoryEventLog::new());
    let session_id = agentyk::SessionId::new();
    let (state, effects) =
        agentyk::TurnState::start(session_id, 4, &agentyk::Message::user("recover this turn"));
    let requests = effects
        .into_iter()
        .map(|data| agentyk::EventRequest::with_turn(session_id, state.turn_id, data))
        .collect();
    log.append_batch(session_id, agentyk::ExpectedVersion::Exact(0), requests)
        .await?;

    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .provider(Provider::llmsim(SimDriver::new([SimTurn::text(
            "recovered",
        )])))
        .build()?;
    let mut session = agent.resume_session(log, session_id).await?;
    let result = session
        .resume_pending()
        .await?
        .expect("turn should be incomplete");
    assert_eq!(result.turn_id, state.turn_id);
    assert_eq!(result.response, "recovered");
    assert!(session.resume_pending().await?.is_none());
    assert_eq!(session.messages().len(), 2);
    Ok(())
}
