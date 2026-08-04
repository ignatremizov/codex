//! Current source metadata, live/cold projection, and receipt lifetime contracts.

use super::*;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn observed_context_preserves_exact_source_and_live_cold_transcript_identity() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let before_turn = session.state.lock().await.last_started_turn_id.clone();
    let (communication, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    persist_context(&session, communication.clone(), commit, accepted)
        .await
        .expect("publication");
    let canonical = canonical_context(
        store.as_ref(),
        &session,
        communication.id.as_ref().expect("id"),
    )
    .await;
    let turn_id = canonical
        .item
        .turn_id()
        .expect("persisted receiver turn")
        .to_string();
    uuid::Uuid::parse_str(&turn_id).expect("stable history-only turn UUID");
    let ResponseItem::AgentMessage {
        internal_chat_message_metadata_passthrough: Some(passthrough),
        ..
    } = &canonical.item
    else {
        panic!("stamped observed response");
    };
    let expected_metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some(turn_id.clone()),
        create_time: Some(passthrough.create_time.clone().expect("recording time")),
        ..Default::default()
    };
    assert_eq!(passthrough, &expected_metadata);
    let mut expected_response = communication.to_model_input_item();
    let ResponseItem::AgentMessage {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut expected_response
    else {
        panic!("typed observed input");
    };
    *internal_chat_message_metadata_passthrough = Some(expected_metadata);
    assert_eq!(canonical.item, expected_response);
    let source = canonical
        .metadata
        .as_ref()
        .expect("canonical harness metadata");
    assert!(source.user_input_order.is_some());
    assert_eq!(
        source,
        &codex_history::CodexHarnessMetadata {
            user_input_order: source.user_input_order,
            mcp_attribution: Some(
                session
                    .services
                    .executed_tool_calls
                    .mcp_attribution_snapshot(),
            ),
            ..Default::default()
        },
        "delivery preserves ordering and attribution without inventing a human authorization source",
    );
    {
        let state = session.state.lock().await;
        assert_eq!(
            state.history.annotated_items(),
            std::slice::from_ref(&canonical)
        );
        assert_eq!(
            state
                .acknowledged_completion_contexts
                .iter()
                .map(|context| (context.item.clone(), context.pending))
                .collect::<Vec<_>>(),
            vec![(canonical.clone(), false)]
        );
        assert_eq!(state.last_started_turn_id, before_turn);
    }
    assert!(session.active_turn.lock().await.is_none());

    let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), events.recv())
        .await
        .expect("raw publication")
        .expect("event channel");
    assert_eq!(event.id, turn_id);
    let EventMsg::RawResponseItem(raw) = &event.msg else {
        panic!("expected one raw source event");
    };
    assert_eq!(raw.item, canonical.item);
    assert!(events.try_recv().is_err());
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical history");
    let mut live = ThreadHistoryBuilder::new();
    live.handle_event(&event.msg);
    let mut cold = ThreadHistoryBuilder::new();
    for item in &history.items {
        cold.handle_rollout_item(item);
    }
    assert_eq!(live.finish(), cold.finish());
}

#[tokio::test]
async fn observed_commentary_is_retained_until_the_successful_compaction_request_covers_it() {
    let (mut session, _, _events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let (communication, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    persist_context(&session, communication.clone(), commit, accepted)
        .await
        .expect("publication");
    let canonical = canonical_context(
        store.as_ref(),
        &session,
        communication.id.as_ref().expect("id"),
    )
    .await;
    for covered in [false, true] {
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let installed = session
            .publish_compacted_history(
                Vec::new(),
                /*reference_context_item*/ None,
                /*world_state_baseline*/ None,
                crate::compact::CompactedHistoryMetadata {
                    completion_source_items: if covered {
                        crate::compact::completion_source_items(std::slice::from_ref(
                            &canonical.item,
                        ))
                    } else {
                        Vec::new()
                    },
                    message: "accepted checkpoint".to_string(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
            .expect("checkpoint");
        let retained = if covered {
            Vec::new()
        } else {
            vec![canonical.clone()]
        };
        assert_eq!(
            installed
                .iter()
                .filter(|item| item.id() == canonical.id())
                .cloned()
                .collect::<Vec<_>>(),
            retained
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .acknowledged_completion_contexts
                .iter()
                .map(|context| context.item.clone())
                .collect::<Vec<_>>(),
            retained
        );
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "The test holds session state to block installation after canonical append while abandoning the caller's receipt."
)]
async fn dropping_snapshot_receipt_does_not_quarantine_its_independent_committed_worker() {
    let (mut session, _, _events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let (_, commit) = observed_communication(&session);
    let snapshots = session
        .services
        .local_agent_runtime
        .control(session.session_id())
        .deferred_response_observation_commit_snapshots(&commit);
    let state = session.state.lock().await;
    let mut receipt = Box::pin(session.persist_agent_response_observations(&snapshots));
    assert!(futures::poll!(receipt.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while store.calls().await.append_completion_items_and_flush == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("independent writer reached canonical append");
    drop(receipt);
    drop(state);
    session.await_history_publication().await;
    assert!(session.check_history_publication().is_ok());
    assert!(!session.submission_admission.requires_reload());
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical snapshot history");
    let actual = history
        .items
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::AgentResponseObservation(observation) => Some(observation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, snapshots);
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
}

#[tokio::test]
async fn never_polled_observed_consumer_quarantines_without_installing_or_retrying() {
    let (session, _, _events) = make_session_and_context_with_rx().await;
    let (communication, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let receipt = session
        .register_communication_delivery(commit, accepted)
        .expect("registered");
    session
        .enqueue_registered_observed_communication(
            communication.clone(),
            TurnStartOptions::default(),
        )
        .await
        .expect("queued");
    let changed = session.input_queue.completion_commit_changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let mut consumer = Box::pin(
        session.consume_observed_communication(&communication, /*recording_turn_id*/ None),
    );
    let waiting = {
        let _entered = runtime.enter();
        futures::poll!(consumer.as_mut()).is_pending()
    };
    drop(consumer);
    runtime.shutdown_background();
    assert!(waiting);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), changed)
        .await
        .expect("drain notified");
    assert!(session.check_history_publication().is_err());
    assert!(session.submission_admission.requires_reload());
    assert!(
        session
            .state
            .lock()
            .await
            .acknowledged_completion_contexts
            .is_empty()
    );
    assert!(
        session
            .consume_observed_communication(&communication, /*recording_turn_id*/ None)
            .await
    );
    assert!(receipt.recv().await.is_err());
}

#[test_case::test_case(false; "background waits for terminal")]
#[test_case::test_case(true; "finishing recorder drains its own context")]
#[tokio::test]
async fn finishing_input_consumption_does_not_wait_for_its_own_terminal(finishing_recorder: bool) {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    session.record_started_turn(&turn.sub_id).await;
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        terminal_pending: true,
        ..Default::default()
    });
    let (communication, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let receipt = session
        .register_communication_delivery(commit, accepted)
        .expect("registered");
    session
        .enqueue_registered_observed_communication(
            communication.clone(),
            TurnStartOptions::default(),
        )
        .await
        .expect("queue accepted observed input");
    let recording_turn_id = finishing_recorder.then_some(turn.sub_id.as_str());
    let mut consumption =
        Box::pin(session.consume_observed_communication(&communication, recording_turn_id));
    if !finishing_recorder {
        assert!(
            tokio::time::timeout(Duration::from_millis(/*millis*/ 20), &mut consumption)
                .await
                .is_err()
        );
        assert!(
            session
                .state
                .lock()
                .await
                .acknowledged_completion_contexts
                .is_empty()
        );
        *session.active_turn.lock().await = None;
        session.active_turn_transition.notify_waiters();
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), consumption)
            .await
            .expect("context publication completes")
    );
    receipt.recv().await.expect("canonical receipt");
    let canonical = canonical_context(
        store.as_ref(),
        &session,
        communication.id.as_ref().expect("id"),
    )
    .await;
    if finishing_recorder {
        assert_eq!(canonical.item.turn_id(), Some(turn.sub_id.as_str()));
        assert!(
            session
                .active_turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|active| active.terminal_pending && active.task.is_none())
        );
    } else {
        assert_ne!(canonical.item.turn_id(), Some(turn.sub_id.as_str()));
        assert!(session.active_turn.lock().await.is_none());
    }
    assert!(session.check_history_publication().is_ok());
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical context");
    assert!(!history.items.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_))
    )));
}
