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
    assert!(session.consume_observed_communication(&communication).await);
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    assert_eq!(
        session
            .state
            .lock()
            .await
            .acknowledged_completion_contexts
            .iter()
            .map(|context| context.item.item.clone())
            .collect::<Vec<_>>(),
        vec![communication.to_model_input_item()],
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
                .any(|context| context.item.item == communication.to_model_input_item())
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
