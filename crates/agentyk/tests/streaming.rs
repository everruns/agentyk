//! Streaming deltas: ephemeral, delivered to listeners, never persisted.

use std::sync::Arc;
use std::sync::Mutex;

use agentyk::{Agent, Event, EventData, EventListener, ModelSpec, Result, SimDriver, SimTurn};
use async_trait::async_trait;
use serde_json::json;

#[derive(Default)]
struct CollectingListener {
    events: Mutex<Vec<Event>>,
}

#[async_trait]
impl EventListener for CollectingListener {
    async fn on_event(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn deltas_reach_listeners_but_never_the_log() -> Result<()> {
    let listener: Arc<CollectingListener> = Arc::default();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("hello there")]))
        .listener_arc(listener.clone())
        .build()?;

    let mut session = agent.session();
    session.run("hi").await?;

    // The sim driver's default streaming behavior reports one delta per
    // reason call, carrying the full text (see ChatDriver::complete_streaming's
    // default impl) — so the listener sees exactly one delta here.
    let seen = listener.events.lock().unwrap().clone();
    let deltas: Vec<&Event> = seen
        .iter()
        .filter(|e| matches!(e.data, EventData::OutputMessageDelta { .. }))
        .collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].sequence, None,
        "ephemeral events carry no sequence"
    );
    if let EventData::OutputMessageDelta {
        delta, accumulated, ..
    } = &deltas[0].data
    {
        assert_eq!(delta, "hello there");
        assert_eq!(accumulated, "hello there");
    }

    // The listener also sees the started/completed durable pair around it.
    let started = seen
        .iter()
        .filter(|e| matches!(e.data, EventData::OutputMessageStarted { .. }))
        .count();
    assert_eq!(started, 1);

    // But the persisted log has NO delta events at all — every persisted
    // event is durable.
    let persisted = session.events().await?;
    assert!(
        !persisted
            .iter()
            .any(|e| matches!(e.data, EventData::OutputMessageDelta { .. }))
    );
    assert!(persisted.iter().all(|e| e.sequence.is_some()));
    Ok(())
}

#[tokio::test]
async fn started_event_carries_model_and_increasing_iteration() -> Result<()> {
    let listener: Arc<CollectingListener> = Arc::default();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("noop", json!({})),
            SimTurn::text("done"),
        ]))
        .listener_arc(listener.clone())
        .build()?;

    agent.run("go").await?;

    let started: Vec<(Option<String>, Option<u32>)> = listener
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.data {
            EventData::OutputMessageStarted {
                model, iteration, ..
            } => Some((model.clone(), *iteration)),
            _ => None,
        })
        .collect();

    assert_eq!(
        started,
        vec![
            (Some("llmsim".to_string()), Some(1)),
            (Some("llmsim".to_string()), Some(2)),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn output_message_events_correlate_by_message_id() -> Result<()> {
    use agentyk::MessageId;

    let listener: Arc<CollectingListener> = Arc::default();
    let agent = Agent::builder()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("hello there")]))
        .listener_arc(listener.clone())
        .build()?;

    agent.run("hi").await?;

    // One reason step → one started, one delta, one completed, all sharing a
    // single message_id (the streaming-lifecycle correlation, gap 1).
    let seen = listener.events.lock().unwrap().clone();
    let ids: Vec<MessageId> = seen
        .iter()
        .filter_map(|e| match &e.data {
            EventData::OutputMessageStarted { message_id, .. }
            | EventData::OutputMessageDelta { message_id, .. }
            | EventData::OutputMessageCompleted { message_id, .. } => Some(*message_id),
            _ => None,
        })
        .collect();

    assert_eq!(ids.len(), 3, "started + delta + completed");
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "all output.message.* events of one reason step share a message_id: {ids:?}"
    );
    Ok(())
}
