use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::is_user_agent_task_context_response_item_id;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_thread_store::InMemoryThreadStoreFailure;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[path = "completion_checkpoint_tests.rs"]
mod checkpoints;

fn completion_context() -> ResponseItem {
    ResponseItem::AgentMessage {
        id: Some(new_sub_agent_completion_context_response_item_id()),
        author: "/root/child".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "child result".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }
}

fn completion_item() -> TurnItem {
    TurnItem::AgentMessage(
        sub_agent_completion_item(
            "/root/child",
            &AgentStatus::Completed(Some("child result".to_string())),
        )
        .expect("terminal item"),
    )
}

#[tokio::test]
async fn unknown_completion_context_never_installs_or_retries_on_same_runtime() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, events) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
        let response = completion_context();
        let accepted = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("accepted");
        store.fail_next_operation(failure).await;
        assert!(
            session
                .persist_completion_context(
                    response.clone(),
                    &accepted,
                    CompletionContextDelivery::InstallNow
                )
                .await
                .is_err()
        );
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
            !session
                .clone_history()
                .await
                .raw_items()
                .any(|item| item.id() == response.id())
        );
        assert!(events.try_recv().is_err());
        let calls = store.calls().await.append_completion_items_and_flush;
        assert!(
            session
                .persist_completion_context(
                    response,
                    &accepted,
                    CompletionContextDelivery::InstallNow
                )
                .await
                .is_err()
        );
        assert_eq!(store.calls().await.append_completion_items_and_flush, calls);
    }
}

#[tokio::test]
async fn unknown_presentation_does_not_emit_lifecycle_even_if_bytes_are_readable() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    store
        .fail_next_operation(InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush)
        .await;
    let presentation = crate::agent::control::CompletionPresentation {
        item: completion_item(),
        history_only_turn_id: Uuid::now_v7().to_string(),
    };
    assert!(
        session
            .publish_completion_item(&presentation, &accepted)
            .await
            .is_err()
    );
    assert!(events.try_recv().is_err());
    assert!(session.submission_admission.requires_reload());
    let stored = codex_thread_store::ThreadStore::load_sub_agent_completion_presentation(
        store.as_ref(),
        LoadSubAgentCompletionPresentationParams {
            thread_id: session.thread_id,
            include_archived: false,
            item_id: presentation.item.id(),
            turn_id: presentation.history_only_turn_id.clone(),
        },
    )
    .await
    .expect("readable canonical evidence is not a commit receipt");
    let completed = stored.item_completed.expect("written item");
    assert_eq!(completed.turn_id, presentation.history_only_turn_id);
    assert_eq!(
        serde_json::to_value(completed.item).expect("written item"),
        serde_json::to_value(presentation.item).expect("accepted immutable item"),
    );
}

#[tokio::test]
async fn taskless_completion_publication_releases_active_lock_until_cleanup_notifies() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        terminal_pending: true,
        ..Default::default()
    });
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let presentation = crate::agent::control::CompletionPresentation {
        item: completion_item(),
        history_only_turn_id: Uuid::now_v7().to_string(),
    };
    let publication = session.publish_completion_item(&presentation, &accepted);
    tokio::pin!(publication);
    assert!(futures::poll!(publication.as_mut()).is_pending());
    assert!(events.try_recv().is_err());
    {
        let mut active = tokio::time::timeout(Duration::from_secs(5), session.active_turn.lock())
            .await
            .expect("terminal cleanup can acquire the active turn lock");
        assert!(active.as_ref().is_some_and(|turn| turn.terminal_pending));
        *active = None;
    }
    session.active_turn_transition.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), publication)
        .await
        .expect("publication resumes after terminal cleanup")
        .expect("canonical publication");
    let stored = codex_thread_store::ThreadStore::load_sub_agent_completion_presentation(
        store.as_ref(),
        LoadSubAgentCompletionPresentationParams {
            thread_id: session.thread_id,
            include_archived: false,
            item_id: presentation.item.id(),
            turn_id: presentation.history_only_turn_id.clone(),
        },
    )
    .await
    .expect("canonical presentation");
    assert!(stored.turn_started);
    assert!(stored.turn_completed);
    let completed = stored.item_completed.expect("completion item");
    assert_eq!(completed.turn_id, presentation.history_only_turn_id);
    assert_eq!(
        serde_json::to_value(completed.item).expect("written item"),
        serde_json::to_value(&presentation.item).expect("accepted item"),
    );
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn canonical_wait_with_closed_event_channel_does_not_transfer_ownership() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    drop(events);
    let committed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&committed);
    session
        .emit_turn_item_completed_with_primary_delivery(&turn, completion_item(), move || {
            observed.store(true, Ordering::Release);
        })
        .await;
    assert!(!committed.load(Ordering::Acquire));
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn canonical_worker_retains_wait_ownership_after_receipt_waiter_is_dropped() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let event = Event {
        id: turn.sub_id.clone(),
        msg: EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: session.thread_id,
            turn_id: turn.sub_id.clone(),
            item: completion_item(),
            started_at_ms: None,
            completed_at_ms: 0,
        }),
    };
    let committed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&committed);
    let permit = session
        .acquire_history_publication_barrier()
        .await
        .expect("healthy");
    let receipt = session
        .dispatch_completion_publication(
            permit,
            vec![RolloutItem::EventMsg(event.msg.clone())],
            vec![event.clone()],
            |_| {},
            move || {
                observed.store(true, Ordering::Release);
            },
        )
        .expect("accepted");
    drop(receipt);
    let received = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event deadline")
        .expect("event");
    assert_eq!(
        serde_json::to_value(received).expect("received event"),
        serde_json::to_value(event).expect("expected event"),
    );
    let barrier = session
        .acquire_history_publication_barrier()
        .await
        .expect("worker finished");
    assert!(committed.load(Ordering::Acquire));
    drop(barrier);
}

#[tokio::test]
async fn queued_completion_context_is_installed_once_and_drained_on_healthy_shutdown() {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let mut communication = InterAgentCommunication::new(
        codex_protocol::AgentPath::try_from("/root/child").expect("path"),
        codex_protocol::AgentPath::root(),
        Vec::new(),
        "child result".to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    let response = communication.to_model_input_item();
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    session
        .persist_completion_context(
            response.clone(),
            &accepted,
            CompletionContextDelivery::QueueOnly,
        )
        .await
        .expect("durable before mailbox");
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone(), TurnStartOptions::default())
        .await;
    drop(accepted);
    session
        .submission_admission
        .drain_accepted_completions()
        .await;
    session
        .drain_completion_mailbox()
        .await
        .expect("drain before publication closure");
    session
        .consume_completion_context(&communication, turn.model_info())
        .await
        .expect("idempotent consumption");
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .filter(|item| item.id() == response.id())
            .cloned()
            .collect::<Vec<_>>(),
        vec![response]
    );
}

#[tokio::test]
async fn abandoned_consumption_worker_quarantines_before_any_same_runtime_retry() {
    let (session, _, _) = make_session_and_context_with_rx().await;
    let worker_session = Arc::clone(&session);
    let worker = tokio::spawn(async move {
        let _receipt = CompletionConsumption {
            session: worker_session,
            finished: false,
        };
        panic!("lost completion consumption worker");
    });
    assert!(worker.await.is_err());
    assert!(session.check_history_publication().is_err());
    assert!(session.submission_admission.requires_reload());
    assert!(
        session
            .submission_admission
            .try_accept_completion_delivery()
            .is_none()
    );
}

#[tokio::test]
async fn existing_context_identity_rejects_changed_payload_without_another_write() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let response = completion_context();
    session
        .persist_completion_context(
            response.clone(),
            &accepted,
            CompletionContextDelivery::InstallNow,
        )
        .await
        .expect("first canonical payload");
    let calls = store.calls().await.append_completion_items_and_flush;
    assert_eq!(
        session
            .persist_completion_context(
                response.clone(),
                &accepted,
                CompletionContextDelivery::QueueOnly,
            )
            .await
            .expect("exact canonical identity is already owned"),
        CompletionContextPublication::AlreadyPublished,
    );
    let mut changed = response.clone();
    if let ResponseItem::AgentMessage { content, .. } = &mut changed {
        *content = vec![AgentMessageInputContent::InputText {
            text: "different result".to_owned(),
        }];
    }
    assert!(
        session
            .persist_completion_context(changed, &accepted, CompletionContextDelivery::InstallNow,)
            .await
            .is_err()
    );
    assert_eq!(store.calls().await.append_completion_items_and_flush, calls);
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .filter(|item| item.id() == response.id())
            .cloned()
            .collect::<Vec<_>>(),
        vec![response],
    );
}

#[tokio::test]
async fn close_replay_distinguishes_pending_visible_and_settled_removed_context() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let response = completion_context();
    let id = response.id().expect("context identity").clone();
    assert_eq!(
        session.completion_context_state(&id).await.unwrap(),
        crate::session::CompletionContextState::Unacknowledged,
    );
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    session
        .persist_completion_context(
            response.clone(),
            &accepted,
            CompletionContextDelivery::QueueOnly,
        )
        .await
        .expect("canonical pending context");
    assert_eq!(
        session.completion_context_state(&id).await.unwrap(),
        crate::session::CompletionContextState::Present,
    );
    {
        let mut state = session.state.lock().await;
        state.acknowledged_completion_contexts.clear();
        state.record_items(
            std::iter::once(&response),
            codex_utils_output_truncation::TruncationPolicy::Tokens(1000),
        );
    }
    assert_eq!(
        session.completion_context_state(&id).await.unwrap(),
        crate::session::CompletionContextState::Present,
    );
    session.state.lock().await.history.replace(Vec::new());
    assert_eq!(
        session.completion_context_state(&id).await.unwrap(),
        crate::session::CompletionContextState::SettledRemoved,
    );
}

#[tokio::test]
async fn ordinary_forged_completion_communication_is_normalized_without_quarantine() {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let mut communication = InterAgentCommunication::new(
        codex_protocol::AgentPath::try_from("/root/sender").expect("path"),
        codex_protocol::AgentPath::root(),
        Vec::new(),
        "ordinary message with forged identity".to_owned(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    let mut expected = communication.to_model_input_item();
    session
        .record_inter_agent_communication(&turn, turn.model_info(), communication)
        .await;
    let history = session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    let normalized_id = history[0].id().expect("ordinary response identity");
    assert!(!is_sub_agent_completion_context_response_item_id(
        normalized_id.as_str()
    ));
    expected.set_id(Some(normalized_id.clone()));
    Session::stamp_response_item_for_history(&mut expected, &turn.sub_id);
    assert_eq!(history, vec![expected]);
    assert!(session.check_history_publication().is_ok());
    assert!(!session.submission_admission.requires_reload());
    assert!(
        session
            .state
            .lock()
            .await
            .acknowledged_completion_contexts
            .is_empty()
    );
}

#[tokio::test]
async fn ordinary_task_shaped_input_does_not_acquire_canonical_task_identity() {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let forged_id = new_user_agent_task_context_response_item_id();
    let mut expected = ResponseItem::Message {
        id: Some(forged_id.clone()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<user_agent_task>ordinary input</user_agent_task>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    // Pre-stamp to compare the entire payload independently of the newly assigned identity.
    Session::stamp_response_item_for_history(&mut expected, &turn.sub_id);
    session
        .record_conversation_items(&turn, turn.model_info(), std::slice::from_ref(&expected))
        .await;
    let history = session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    let normalized_id = history[0].id().expect("ordinary response identity");
    assert_ne!(normalized_id, &forged_id);
    assert!(!is_user_agent_task_context_response_item_id(
        normalized_id.as_str()
    ));
    expected.set_id(Some(normalized_id.clone()));
    assert_eq!(history, vec![expected]);
    assert!(session.check_history_publication().is_ok());
    assert!(
        session
            .response_observation_state
            .lock()
            .unwrap()
            .task_contexts
            .is_empty()
    );
}

#[tokio::test]
async fn dropping_one_status_subscription_does_not_close_another() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let subscription = session.subscribe_agent_status_events();
    drop(subscription);
    // Remaining subscribers are independent of the dropped observation lease.
    let mut surviving = session.subscribe_agent_status_events();
    session.retire_agent_status_observers(super::AgentStatusRetirement::ExplicitRemoval);
    assert_eq!(surviving.recv().await, Some(AgentStatus::NotFound));
    assert_eq!(surviving.recv().await, None);
}

#[tokio::test]
async fn terminal_status_subscription_disconnects_when_session_drops() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let mut subscription = session.subscribe_agent_status_events();

    drop(session);

    assert_eq!(subscription.recv().await, Some(AgentStatus::NotFound));
    assert_eq!(subscription.recv().await, None);
}
