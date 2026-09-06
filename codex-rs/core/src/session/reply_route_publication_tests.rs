use super::*;
use crate::context::AgentContextIdentity;
use crate::context::AgentReplyRoute;
use crate::context::ContextualUserFragment;
use crate::session::CompactedHistoryMetadata;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::ThreadId;
use codex_thread_store::InMemoryThreadStoreFailure;
use futures::poll;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn route(source: ThreadId) -> ResponseItem {
    ContextualUserFragment::into(AgentReplyRoute::until_disabled(
        AgentContextIdentity::Canonical { agent_id: source },
    ))
}

#[tokio::test]
async fn stale_checkpoint_retains_acknowledged_routes_without_importing_new_sources() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let _store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let source = ThreadId::new();
    session
        .publish_persistent_reply_route(route(source))
        .await
        .unwrap();
    let accepted = session.clone_history().await.annotated_items().to_vec();
    for replacement in [
        Vec::new(),
        vec![
            ResponseItemEnvelope::new(route(source)),
            ResponseItemEnvelope::new(route(ThreadId::new())),
        ],
    ] {
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let installed = session
            .replace_compacted_history(
                replacement,
                /*reference_context_item*/ None,
                /*world_state_baseline*/ None,
                CompactedHistoryMetadata {
                    completion_source_items: Vec::new(),
                    message: "summary".to_string(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(installed, accepted);
        assert_eq!(session.clone_history().await.annotated_items(), accepted);
    }
}

#[tokio::test]
async fn repeated_acknowledged_route_reuses_canonical_singleton() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let source = ThreadId::new();
    session
        .publish_persistent_reply_route(route(source))
        .await
        .unwrap();
    let accepted = session.clone_history().await.annotated_items().to_vec();
    session
        .publish_persistent_reply_route(route(source))
        .await
        .unwrap();
    assert_eq!(session.clone_history().await.annotated_items(), accepted);
    assert_eq!(accepted.len(), 1);
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
}

#[tokio::test]
async fn lost_route_receipt_quarantines_without_installing_or_retrying() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, _) = make_session_and_context_with_rx().await;
        let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
        let source = ThreadId::new();
        store.fail_next_operation(failure).await;
        assert!(
            session
                .publish_persistent_reply_route(route(source))
                .await
                .is_err()
        );
        assert!(session.submission_admission.requires_reload());
        assert!(session.clone_history().await.annotated_items().is_empty());
        let calls = store.calls().await.append_completion_items_and_flush;
        assert!(
            session
                .publish_persistent_reply_route(route(source))
                .await
                .is_err()
        );
        assert_eq!(store.calls().await.append_completion_items_and_flush, calls);
    }
}

#[tokio::test]
async fn route_worker_survives_cancellation_of_its_waiter() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let source = ThreadId::new();
    let permit = session.reserve_history_publication().await;
    let mut pending = Box::pin(session.publish_persistent_reply_route(route(source)));
    assert!(poll!(&mut pending).is_pending());
    drop(pending);
    drop(permit);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if session
                .clone_history()
                .await
                .annotated_items()
                .iter()
                .any(|item| {
                    codex_history::persistent_agent_reply_route_source(item) == Some(source)
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    assert!(!session.submission_admission.requires_reload());
}
