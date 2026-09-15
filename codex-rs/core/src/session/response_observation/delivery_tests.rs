use super::*;
use crate::agent::control::ResponseObservationDeliveryKind;
use crate::agent::control::SessionPresentationId;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[path = "delivery_integration_tests.rs"]
mod integration;

async fn canonical_context(
    store: &dyn ThreadStore,
    session: &Session,
    id: &ResponseItemId,
) -> codex_history::ResponseItemEnvelope {
    store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical observed context")
        .segments
        .into_iter()
        .flatten()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(envelope) if envelope.id() == Some(id) => Some(envelope),
            _ => None,
        })
        .expect("canonical source envelope")
}

fn observed_communication(
    session: &Session,
) -> (InterAgentCommunication, ResponseObservationDeliveryCommit) {
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/child").expect("child path"),
        AgentPath::try_from("/root").expect("parent path"),
        Vec::new(),
        "complete commentary".to_string(),
        /*trigger_turn*/ false,
    );
    let response_item_id = ResponseItemId::new("amsg");
    communication.id = Some(response_item_id.clone());
    let commit = ResponseObservationDeliveryCommit {
        parent: session.presentation_id(),
        child: SessionPresentationId::new(ThreadId::new(), uuid::Uuid::now_v7()),
        turn_id: "actual-child-turn".to_string(),
        response_item_id,
        kind: ResponseObservationDeliveryKind::Commentary,
        mailbox_final_subscription_message_id: None,
        model_visibility: codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
    };
    (communication, commit)
}

async fn persist_context(
    session: &Arc<Session>,
    communication: InterAgentCommunication,
    commit: ResponseObservationDeliveryCommit,
    accepted: AcceptedCompletionDelivery,
) -> CodexResult<()> {
    session
        .persist_observation_payload(
            commit,
            Arc::new(accepted),
            Payload::Context {
                communication,
                presentation: None,
                recording_turn_id: None,
            },
        )
        .await
}

#[tokio::test]
async fn ambiguous_observed_barrier_never_installs_or_retries_readable_evidence() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, events) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
        let (communication, commit) = observed_communication(&session);
        let accepted = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("accepted");
        let later = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("accepted");
        store.fail_next_operation(failure).await;
        assert!(
            persist_context(&session, communication.clone(), commit.clone(), accepted,)
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
        assert!(events.try_recv().is_err());
        let before = store.calls().await.append_completion_items_and_flush;
        // Reading a full or partial raw prefix cannot acknowledge the failed barrier.
        let _evidence = store
            .load_canonical_artifact_segments(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("raw evidence");
        assert!(
            persist_context(&session, communication, commit, later)
                .await
                .is_err()
        );
        assert_eq!(
            store.calls().await.append_completion_items_and_flush,
            before
        );
    }
}

#[tokio::test]
async fn dropping_a_queued_receipt_preserves_exactly_one_consumption() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
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
    drop(receipt);
    session.submission_admission.close_completion_admission();
    session.drain_observed_communications().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        session.submission_admission.drain_accepted_completions(),
    )
    .await
    .expect("accepted worker drained");
    assert!(
        session
            .consume_observed_communication(&communication, /*recording_turn_id*/ None)
            .await
    );
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    let canonical = canonical_context(
        store.as_ref(),
        &session,
        communication.id.as_ref().expect("id"),
    )
    .await;
    assert_eq!(
        session
            .state
            .lock()
            .await
            .acknowledged_completion_contexts
            .iter()
            .map(|context| context.item.clone())
            .collect::<Vec<_>>(),
        vec![canonical]
    );
    let evidence = store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical envelope");
    assert!(evidence.segments.iter().any(|segment| {
        segment
            .iter()
            .enumerate()
            .any(|(index, _)| codex_history::is_committed_observed_response(segment, index))
    }));
}

#[tokio::test]
async fn cancelled_direct_waiter_does_not_abandon_the_canonical_worker() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let (communication, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let permit = session.reserve_history_publication().await;
    {
        let publication = persist_context(&session, communication.clone(), commit, accepted);
        tokio::pin!(publication);
        assert!(futures::poll!(publication.as_mut()).is_pending());
    }
    drop(permit);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if session
                .state
                .lock()
                .await
                .acknowledged_completion_contexts
                .iter()
                .any(|context| context.item.id() == communication.id.as_ref())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned publication completed");
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn passive_final_signal_follows_the_canonical_worker_after_caller_cancellation() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let (mut communication, mut commit) = observed_communication(&session);
    commit.response_item_id =
        codex_protocol::protocol::new_sub_agent_completion_context_response_item_id();
    communication.id = Some(commit.response_item_id.clone());
    commit.kind = ResponseObservationDeliveryKind::Final;
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let mut passive_final_activity = session.subscribe_passive_final_delivery_activity();
    let permit = session.reserve_history_publication().await;
    {
        let publication = persist_context(&session, communication, commit, accepted);
        tokio::pin!(publication);
        assert!(futures::poll!(publication.as_mut()).is_pending());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(/*millis*/ 50),
                passive_final_activity.changed(),
            )
            .await
            .is_err()
        );
    }
    drop(permit);
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        passive_final_activity.changed(),
    )
    .await
    .expect("canonical worker should signal after commit")
    .expect("passive final activity sender should remain open");
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    let evidence = store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical evidence");
    assert!(evidence.segments.iter().any(|segment| {
        segment
            .iter()
            .enumerate()
            .any(|(index, _)| codex_history::is_committed_observed_response(segment, index))
    }));
}

#[tokio::test]
async fn only_new_passive_final_installation_signals_sleep_for_durable_and_ephemeral_sessions() {
    for persistent in [false, true] {
        for (kind, trigger_turn, defer_to_next_turn, expected_activity) in [
            (
                ResponseObservationDeliveryKind::Commentary,
                false,
                false,
                false,
            ),
            (ResponseObservationDeliveryKind::Final, true, false, false),
            (ResponseObservationDeliveryKind::Final, false, true, false),
            (ResponseObservationDeliveryKind::Final, false, false, true),
        ] {
            let (mut session, _, _events) = make_session_and_context_with_rx().await;
            let _store = if persistent {
                Some(
                    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique"))
                        .await,
                )
            } else {
                None
            };
            assert_eq!(session.live_thread().is_some(), persistent);
            let (mut communication, mut commit) = observed_communication(&session);
            commit.response_item_id =
                codex_protocol::protocol::new_sub_agent_completion_context_response_item_id();
            commit.kind = kind;
            communication.id = Some(commit.response_item_id.clone());
            communication.trigger_turn = trigger_turn;
            communication.defer_to_next_turn = defer_to_next_turn;
            let response_id = commit.response_item_id.clone();
            let activity = session.subscribe_passive_final_delivery_activity();
            let accepted = session
                .submission_admission
                .try_accept_completion_delivery()
                .expect("accepted");
            persist_context(&session, communication, commit, accepted)
                .await
                .expect("owned publication");
            assert_eq!(
                activity.has_changed().expect("live signal sender"),
                expected_activity
            );
            {
                let state = session.state.lock().await;
                let envelope = state
                    .completion_publication_receipts
                    .contexts
                    .get(&response_id)
                    .expect("the existing full-envelope receipt owns deduplication");
                assert_eq!(
                    state.history.annotated_items(),
                    std::slice::from_ref(envelope)
                );
                let metadata = envelope
                    .metadata
                    .as_ref()
                    .expect("ordered delivery metadata");
                assert!(metadata.user_input_order.is_some());
                assert_eq!(
                    metadata,
                    &codex_history::CodexHarnessMetadata {
                        user_input_order: metadata.user_input_order,
                        mcp_attribution: Some(
                            session
                                .services
                                .executed_tool_calls
                                .mcp_attribution_snapshot(),
                        ),
                        ..Default::default()
                    },
                    "passive delivery does not manufacture human authorization",
                );
            }
            assert!(
                !session
                    .subscribe_passive_final_delivery_activity()
                    .has_changed()
                    .expect("fresh subscription")
            );
            assert!(
                session.active_turn.lock().await.is_none(),
                "the hint cannot start an idle turn"
            );
        }
    }
}

#[tokio::test]
async fn dropping_an_unqueued_receipt_releases_admission() {
    let (session, _, _) = make_session_and_context_with_rx().await;
    let (_, commit) = observed_communication(&session);
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let receipt = session
        .register_communication_delivery(commit, accepted)
        .expect("registered");
    drop(receipt);
    tokio::time::timeout(
        Duration::from_secs(5),
        session.submission_admission.drain_accepted_completions(),
    )
    .await
    .expect("unqueued claim released");
}
