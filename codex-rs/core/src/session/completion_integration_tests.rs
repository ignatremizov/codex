//! Current publication metadata and lifecycle seams used by the background-completion owner.

use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::sync::Mutex;

struct ItemRecorder(Arc<Mutex<Vec<TurnItem>>>);

impl TurnLifecycleContributor for ItemRecorder {
    fn on_item_completed<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a TurnItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.0.lock().expect("item observations").push(item.clone());
        })
    }
}

#[tokio::test]
async fn canonical_wait_preserves_start_timing_item_hooks_and_legacy_delivery() -> anyhow::Result<()> {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session"))
        .await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.turn_lifecycle_contributor(Arc::new(ItemRecorder(Arc::clone(&observed))));
    Arc::get_mut(&mut session).expect("unique session").services.extensions =
        Arc::new(extensions.build());
    let child_id = ThreadId::new();
    let item = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: "wait-receipt".to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::Completed,
        deadline_at_ms: None,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: vec![child_id],
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: std::collections::HashMap::from([(
            child_id, AgentStatus::Completed(Some("child result".to_string())),
        )]),
        completion_presentation_agent_ids: Some(vec![child_id]),
    });
    let started_at_ms = turn.turn_timing_state
        .record_item_started(item.id(), /*started_at_ms*/ 123)
        .await;
    let committed = Arc::new(AtomicBool::new(false));
    let committed_by_worker = Arc::clone(&committed);
    session.emit_turn_item_completed_with_primary_delivery(&turn, item.clone(), move || {
        committed_by_worker.store(true, Ordering::Release);
    }).await;

    let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), events.recv()).await??;
    let EventMsg::ItemCompleted(completed) = &event.msg else {
        anyhow::bail!("expected canonical item receipt, got {event:?}");
    };
    let expected = ItemCompletedEvent {
        thread_id: session.thread_id,
        turn_id: turn.sub_id.clone(),
        item: item.clone(),
        started_at_ms: Some(started_at_ms),
        completed_at_ms: completed.completed_at_ms,
    };
    assert_eq!(serde_json::to_value(completed)?, serde_json::to_value(&expected)?);
    assert!(committed.load(Ordering::Acquire));
    assert_eq!(
        serde_json::to_value(&*observed.lock().expect("item observations"))?,
        serde_json::to_value(vec![item.clone()])?,
    );
    assert_eq!(turn.turn_timing_state.take_item_started(&item.id()).await, None);
    let legacy = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), events.recv()).await??;
    assert_eq!(
        serde_json::to_value(&legacy.msg)?,
        serde_json::to_value(event.msg.as_legacy_events(/*show_raw_agent_reasoning*/ false)
            .first().expect("wait legacy mirror"))?,
    );
    assert!(events.try_recv().is_err());
    let history = store.load_history(LoadThreadHistoryParams {
        thread_id: session.thread_id,
        include_archived: false,
    }).await?;
    let canonical = history.items.into_iter().filter_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::ItemCompleted(item)) if item.item.id() == "wait-receipt" => Some(item),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(serde_json::to_value(canonical)?, serde_json::to_value(vec![expected])?);
    Ok(())
}

#[tokio::test]
async fn queued_completion_installs_its_exact_canonical_source_envelope() -> anyhow::Result<()> {
    let (mut session, _, _events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session"))
        .await;
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/child").map_err(anyhow::Error::msg)?,
        AgentPath::root(),
        Vec::new(),
        "canonical source identity".to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    let response = communication.to_model_input_item();
    let reservation = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accept completion");
    session
        .persist_completion_context(
            response.clone(),
            &reservation,
            CompletionContextDelivery::QueueOnly,
        )
        .await?;
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await?;
    let canonical = history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(envelope) if envelope.item == response => {
                Some(envelope.clone())
            }
            _ => None,
        })
        .expect("canonical completion envelope");
    assert_eq!(
        canonical,
        codex_history::ResponseItemEnvelope::new(response.clone()),
        "typed agent delivery retains its complete envelope, not human authorization metadata",
    );
    assert!(
        !session
            .clone_history()
            .await
            .raw_items()
            .any(|item| item.id() == response.id())
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication, TurnStartOptions::default())
        .await;
    drop(reservation);
    session.drain_completion_mailbox().await?;
    let state = session.state.lock().await;
    let live = state
        .history
        .annotated_items()
        .iter()
        .filter(|envelope| envelope.id() == response.id())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(live, vec![canonical.clone()]);
    let acknowledged = state
        .acknowledged_completion_contexts
        .iter()
        .map(|item| (item.item.clone(), item.pending))
        .collect::<Vec<_>>();
    assert_eq!(acknowledged, vec![(canonical, false)]);
    Ok(())
}

#[tokio::test]
async fn unpolled_completion_consumer_retains_abandonment_after_its_waiter_is_dropped()
-> anyhow::Result<()> {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/child").map_err(anyhow::Error::msg)?,
        AgentPath::root(),
        Vec::new(),
        "accepted but not consumed".to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    let response = communication.to_model_input_item();
    let reservation = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accept completion");
    session
        .persist_completion_context(
            response.clone(),
            &reservation,
            CompletionContextDelivery::QueueOnly,
        )
        .await?;
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone(), TurnStartOptions::default())
        .await;
    drop(reservation);
    let (leased, _) = session.input_queue.drain_mailbox_input_items().await;
    assert_eq!(leased.len(), 1);
    let changed = session.input_queue.completion_commit_changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();

    // Enter a distinct current-thread runtime without driving it. The real consumer marks
    // the mailbox Committing and spawns its worker, but the worker cannot be polled here.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut consume =
        Box::pin(session.consume_completion_context(&communication, turn.model_info()));
    let waiting = {
        let _entered = runtime.enter();
        futures::poll!(consume.as_mut()).is_pending()
    };
    drop(consume);
    runtime.shutdown_background();
    assert!(
        waiting,
        "consumer should be waiting on its independently owned worker"
    );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), changed).await?;
    assert!(session.submission_admission.requires_reload());
    assert!(session.check_history_publication().is_err());
    assert!(
        !session
            .clone_history()
            .await
            .raw_items()
            .any(|item| item.id() == response.id())
    );
    Ok(())
}
