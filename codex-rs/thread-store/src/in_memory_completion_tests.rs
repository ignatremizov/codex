use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

use super::InMemoryThreadStore;
use super::InMemoryThreadStoreFailure;
use super::tests::create_thread_params;
use crate::AppendThreadItemsParams;
use crate::LoadSubAgentCompletionContextItemParams;
use crate::LoadSubAgentCompletionPresentationParams;
use crate::ThreadStore;
use crate::ThreadStoreError;

#[tokio::test]
async fn completion_commit_survives_derived_metadata_failure_without_reappending() {
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let store = std::sync::Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let live =
            crate::LiveThread::create(store.clone(), create_thread_params(thread_id, history_mode))
                .await
                .expect("create live thread");
        let presentation = ItemCompletedEvent {
            thread_id,
            turn_id: "completion-turn".to_string(),
            item: TurnItem::AgentMessage(
                sub_agent_completion_item("/root/worker", &AgentStatus::Shutdown)
                    .expect("terminal completion"),
            ),
            started_at_ms: None,
            completed_at_ms: 1,
        };
        let item = RolloutItem::EventMsg(EventMsg::ItemCompleted(presentation.clone()));
        let mut expected = store.state.lock().await.histories[&thread_id].clone();
        expected.push(item.clone());
        store
            .fail_next_operation(InMemoryThreadStoreFailure::ThreadMetadataUpdate)
            .await;

        live.append_completion_items_and_flush_canonical(&[item])
            .await
            .expect("derived metadata failure must not obscure canonical acknowledgement");
        assert_eq!(
            store.calls().await,
            super::InMemoryThreadStoreCalls {
                create_thread: 1,
                append_completion_items_and_flush: 1,
                persist_thread: 1,
                flush_thread: 1,
                update_thread_metadata: 1,
                ..Default::default()
            },
        );
        {
            let state = store.state.lock().await;
            assert_eq!(state.fail_next_operation, None);
            assert!(state.metadata_updates.is_empty());
            assert_eq!(
                serde_json::to_value(&state.histories[&thread_id]).expect("actual history"),
                serde_json::to_value(&expected).expect("expected history"),
            );
        }
        let found = store
            .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
                thread_id,
                include_archived: false,
                item_id: presentation.item.id(),
                turn_id: presentation.turn_id.clone(),
            })
            .await
            .expect("canonical completion remains readable");
        assert_eq!(
            (
                serde_json::to_value(found.item_completed).expect("actual presentation"),
                found.turn_started,
                found.turn_completed,
            ),
            (
                serde_json::to_value(Some(presentation)).expect("expected presentation"),
                false,
                false,
            ),
        );

        // Retrying only the pending metadata must not append the canonical item again.
        live.flush().await.expect("retry derived metadata");
        assert_eq!(
            serde_json::to_value(&store.state.lock().await.histories[&thread_id])
                .expect("history after metadata recovery"),
            serde_json::to_value(expected).expect("expected history"),
        );
        let calls = store.calls().await;
        assert_eq!(
            (
                calls.append_completion_items_and_flush,
                calls.update_thread_metadata
            ),
            (1, 2),
        );
    }
}

#[tokio::test]
async fn empty_live_completion_batch_still_reports_a_failed_store_barrier() {
    let store = std::sync::Arc::new(InMemoryThreadStore::default());
    let thread_id = ThreadId::new();
    let live = crate::LiveThread::create(
        store.clone(),
        create_thread_params(thread_id, ThreadHistoryMode::Paginated),
    )
    .await
    .expect("create live thread");
    store
        .fail_next_operation(InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush)
        .await;
    assert!(
        live.append_completion_items_and_flush_canonical(&[])
            .await
            .is_err()
    );
    assert_eq!(
        store.calls().await,
        super::InMemoryThreadStoreCalls {
            create_thread: 1,
            append_completion_items_and_flush: 1,
            persist_thread: 1,
            flush_thread: 1,
            ..Default::default()
        },
    );
}

#[tokio::test]
async fn completion_barrier_retains_exactly_the_acknowledged_or_unknown_prefix() {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for (failure, retained) in [
            (None, 3),
            (
                Some(InMemoryThreadStoreFailure::SubAgentCompletionAppend),
                0,
            ),
            (
                Some(InMemoryThreadStoreFailure::SubAgentCompletionPrefix),
                1,
            ),
            (
                Some(InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush),
                3,
            ),
        ] {
            let store = InMemoryThreadStore::default();
            let thread_id = ThreadId::new();
            store
                .create_thread(create_thread_params(thread_id, mode))
                .await
                .expect("create");
            let response_id = new_sub_agent_completion_context_response_item_id();
            let response = ResponseItem::AgentMessage {
                id: Some(response_id.clone()),
                author: "/root/worker".to_string(),
                recipient: "/root".to_string(),
                content: vec![AgentMessageInputContent::InputText {
                    text: "done".to_string(),
                }],
                internal_chat_message_metadata_passthrough: None,
            };
            let presentation = ItemCompletedEvent {
                thread_id,
                turn_id: "completion-turn".to_string(),
                item: TurnItem::AgentMessage(
                    sub_agent_completion_item(
                        "/root/worker",
                        &AgentStatus::Completed(Some("done".to_string())),
                    )
                    .expect("terminal"),
                ),
                started_at_ms: None,
                completed_at_ms: 1,
            };
            let items = vec![
                RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                },
                RolloutItem::ResponseItem(response.clone().into()),
                RolloutItem::EventMsg(EventMsg::ItemCompleted(presentation.clone())),
            ];
            let mut expected = store.state.lock().await.histories[&thread_id].clone();
            expected.extend_from_slice(&items[..retained]);
            if let Some(failure) = failure {
                store.fail_next_operation(failure).await;
            }
            let result = store
                .append_completion_items_and_flush(AppendThreadItemsParams { thread_id, items })
                .await;
            assert_eq!(result.is_ok(), failure.is_none());
            // Neither a later flush nor teardown retries a failed completion batch.
            store.flush_thread(thread_id).await.expect("flush");
            store.shutdown_thread(thread_id).await.expect("shutdown");
            assert_eq!(
                serde_json::to_value(&store.state.lock().await.histories[&thread_id])
                    .expect("actual"),
                serde_json::to_value(expected).expect("expected"),
            );
            assert_eq!(store.calls().await.append_completion_items_and_flush, 1);

            let context = store
                .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                    thread_id,
                    include_archived: false,
                    response_item_id: response_id,
                })
                .await
                .expect("context");
            assert_eq!(context, (retained == 3).then_some(response));
            let found = store
                .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
                    thread_id,
                    include_archived: false,
                    item_id: presentation.item.id(),
                    turn_id: presentation.turn_id.clone(),
                })
                .await
                .expect("presentation");
            assert_eq!(
                serde_json::to_value(found.item_completed).expect("serialize actual"),
                serde_json::to_value((retained == 3).then_some(presentation.clone()))
                    .expect("serialize expected"),
            );
            if retained == 3 {
                assert!(matches!(
                    store
                        .load_sub_agent_completion_presentation(
                            LoadSubAgentCompletionPresentationParams {
                                thread_id,
                                include_archived: false,
                                item_id: presentation.item.id(),
                                turn_id: "another-turn".to_string(),
                            },
                        )
                        .await,
                    Err(ThreadStoreError::Conflict { .. })
                ));
            }
        }
    }
}
