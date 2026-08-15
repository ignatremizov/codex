use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::items::UserAgentControlAction;
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
        let (task, snapshot) = task_fixture(&session);
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

fn task_fixture(session: &Session) -> (ResponseItem, AgentResponseObservation) {
    let target_thread_id = codex_protocol::ThreadId::new();
    let mut task =
        crate::context::ContextualUserFragment::into(crate::context::UserAgentTask::new(
            crate::context::AgentContextIdentity::Canonical {
                agent_id: target_thread_id,
            },
            "review target",
        ));
    task.set_id(Some(new_user_agent_task_context_response_item_id()));
    let snapshot = AgentResponseObservation {
        observer_thread_id: session.thread_id,
        target_thread_id,
        target_turn_id: Some("actual-target-turn".to_string()),
        task_preview: None,
        promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(&task),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: Vec::new(),
    };
    (task, snapshot)
}

#[tokio::test]
async fn task_publication_installs_exact_canonical_context_without_user_authorization() {
    use codex_thread_store::ThreadStore;
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    Arc::get_mut(&mut session)
        .expect("unique session")
        .services
        .executed_tool_calls = crate::state::ExecutedToolCalls::new(
        &turn.config.features,
        &codex_history::InitialHistory::New,
    );
    session.services.executed_tool_calls.record_mcp_source(
        codex_protocol::mcp::McpAttributionSource {
            connector_id: None,
            plugin_id: None,
            server_name: "example".into(),
            tool_name: "search".into(),
            first_turn_id: "source-turn".into(),
        },
    );
    let attribution = session
        .services
        .executed_tool_calls
        .mcp_attribution_snapshot();
    let before = session.subscribe_agent_responses().0;
    let last_started = session.state.lock().await.last_started_turn_id.clone();
    let (task, snapshot) = task_fixture(&session);
    let id = task.id().expect("task id").clone();
    let transaction = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    session
        .commit_user_agent_task(transaction, vec![snapshot.clone()], Some(task), || Ok(()))
        .await
        .expect("acknowledged publication");
    let history = store
        .load_history(codex_thread_store::LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical task history");
    let proofs = codex_history::committed_user_agent_task_contexts(&history.items);
    assert_eq!(proofs.len(), 1);
    let proof = proofs.get(&id).expect("exact adjacent task proof");
    assert_eq!(proof.observation, snapshot);
    let source = proof
        .item
        .metadata
        .as_ref()
        .expect("prepared source metadata");
    assert!(source.user_input_order.is_some());
    assert_eq!(source.mcp_attribution, Some(attribution));
    assert!(
        source.retained_source.is_none(),
        "synthetic context must not become human authorization"
    );
    assert!(!crate::context::is_user_authorization_message(
        &proof.item.item
    ));
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &proof.item.item
    else {
        panic!("stamped typed task");
    };
    assert!(metadata.turn_id.is_some());
    assert!(metadata.create_time.is_some());
    assert_eq!(
        metadata.content_item_kinds,
        Some(vec![codex_protocol::models::ContentItemKind(
            "multi_agent.user_agent_task".into(),
        )])
    );
    {
        let state = session.state.lock().await;
        assert_eq!(
            state.history.annotated_items(),
            std::slice::from_ref(&proof.item)
        );
        assert_eq!(state.last_started_turn_id, last_started);
    }
    assert_eq!(
        session
            .response_observation_state
            .lock()
            .expect("live proof")
            .task_contexts[&id]
            .item,
        proof.item
    );
    assert!(
        session
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(/*force*/ false)
            .is_none()
    );
    assert_eq!(session.subscribe_agent_responses().0, before);
    assert!(session.active_turn.lock().await.is_none());
    assert!(
        events.try_recv().is_err(),
        "task promotion is not a source-side model response"
    );
}

#[test_case::test_case(false; "audit")]
#[test_case::test_case(true; "task")]
#[tokio::test]
async fn never_polled_user_publication_requires_reload_without_retries(task_publication: bool) {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let (task, snapshot) = task_fixture(&session);
    let transaction = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    let mut request = Box::pin(async {
        if task_publication {
            session
                .commit_user_agent_task(transaction, vec![snapshot], Some(task), || Ok(()))
                .await
        } else {
            session
                .record_user_agent_control(UserAgentControlItem::succeeded(
                    UserAgentControlAction::Prompt,
                ))
                .await
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    {
        let _entered = runtime.enter();
        assert!(poll!(&mut request).is_pending());
    }
    drop(request);
    runtime.shutdown_background();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while !session.submission_admission.requires_reload() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("abandoned worker quarantines");
    assert!(
        session
            .state
            .lock()
            .await
            .history
            .annotated_items()
            .is_empty()
    );
    assert!(
        session
            .response_observation_state
            .lock()
            .expect("task proofs")
            .task_contexts
            .is_empty()
    );
    assert!(events.try_recv().is_err());
    assert_eq!(store.calls().await.append_completion_items_and_flush, 0);
}
