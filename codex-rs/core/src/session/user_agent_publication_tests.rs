use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_thread_store::InMemoryThreadStoreFailure;
use futures::poll;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test]
async fn audit_does_not_create_a_model_turn_or_change_response_state() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let before = session.subscribe_agent_responses().0;
    let item = UserAgentControlItem::succeeded(UserAgentControlAction::Prompt);
    let id = item.id.clone();
    session.record_user_agent_control(item).await.unwrap();
    let event = events.recv().await.unwrap();
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("audit item")
    };
    assert_eq!(completed.turn_id, id);
    assert!(matches!(completed.item, TurnItem::UserAgentControl(_)));
    assert!(events.try_recv().is_err());
    assert_eq!(session.subscribe_agent_responses().0, before);
    assert!(session.clone_history().await.raw_items().next().is_none());
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
}

#[tokio::test]
async fn audit_worker_survives_cancellation_of_its_result_waiter() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let permit = session.reserve_history_publication().await;
    let mut audit = Box::pin(
        session.record_user_agent_control(UserAgentControlItem::succeeded(
            UserAgentControlAction::Prompt,
        )),
    );
    assert!(poll!(&mut audit).is_pending());
    drop(audit);
    drop(permit);
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event.msg, EventMsg::ItemCompleted(_)));
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn uncertain_audit_append_is_never_retried_or_emitted() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, events) = make_session_and_context_with_rx().await;
        let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
        let item = UserAgentControlItem::succeeded(UserAgentControlAction::Prompt);
        store.fail_next_operation(failure).await;
        assert!(
            session
                .record_user_agent_control(item.clone())
                .await
                .is_err()
        );
        let calls = store.calls().await.append_completion_items_and_flush;
        assert!(session.record_user_agent_control(item).await.is_err());
        assert_eq!(store.calls().await.append_completion_items_and_flush, calls);
        assert!(events.try_recv().is_err());
        assert!(session.submission_admission.requires_reload());
    }
}

#[tokio::test]
async fn uncertain_task_append_never_installs_context_or_observation_policy() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, events) = make_session_and_context_with_rx().await;
        let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
        let task = ResponseItem::Message {
            id: Some(new_user_agent_task_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<user_agent_task>review target</user_agent_task>".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let snapshot = AgentResponseObservation {
            observer_thread_id: session.thread_id,
            target_thread_id: codex_protocol::ThreadId::new(),
            target_turn_id: Some("actual-target-turn".to_string()),
            task_preview: None,
            promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(&task),
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            target_messages: false,
            reply_route_enabled: None,
            reply_route_context_installed: false,
            queue_delivery: false,
            message_wake_turn_id: None,
            baseline_final_delivery: AgentResponseFinalDelivery::Passive,
            final_delivery: AgentResponseFinalDelivery::Wake,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        };
        let installed = Arc::new(AtomicBool::new(false));
        let installation = Arc::clone(&installed);
        let transaction = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
        store.fail_next_operation(failure).await;
        assert!(
            session
                .commit_user_agent_task(
                    transaction,
                    vec![snapshot],
                    Some(task.clone()),
                    move || {
                        installation.store(true, Ordering::Release);
                        Ok(())
                    },
                )
                .await
                .is_err()
        );
        assert!(!installed.load(Ordering::Acquire));
        assert!(
            !session
                .clone_history()
                .await
                .raw_items()
                .any(|item| item.id() == task.id())
        );
        assert!(
            session
                .response_observation_state
                .lock()
                .unwrap()
                .task_contexts
                .is_empty()
        );
        assert!(session.submission_admission.requires_reload());
        assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
        assert!(events.try_recv().is_err());
    }
}
