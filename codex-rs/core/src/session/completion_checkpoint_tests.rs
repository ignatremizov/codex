use super::*;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;

#[tokio::test]
async fn acknowledged_completion_survives_stale_checkpoint_before_or_after_consumption() {
    // A replacement computed before the completion is deliberately reused in both orders.
    for consume_before_checkpoint in [false, true] {
        let (mut session, turn, _) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
        let stale_replacement = Vec::new();
        let mut communication = InterAgentCommunication::new(
            codex_protocol::AgentPath::try_from("/root/child").expect("path"),
            codex_protocol::AgentPath::root(),
            Vec::new(),
            "accepted full result".to_owned(),
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
            .expect("canonical completion receipt");
        assert!(
            !session
                .clone_history()
                .await
                .raw_items()
                .any(|item| item == &response)
        );
        session
            .input_queue
            .enqueue_mailbox_communication(communication.clone(), TurnStartOptions::default())
            .await;
        if consume_before_checkpoint {
            let _ = session.input_queue.drain_mailbox_input_items().await;
            session
                .consume_completion_context(&communication, turn.model_info())
                .await
                .expect("consume");
        }
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let live = session
            .publish_compacted_history(
                stale_replacement,
                /*reference_context_item*/ None,
                /*world_state_baseline*/ None,
                CompactedHistoryMetadata {
                    completion_source_items: Vec::new(),
                    message: "stale model checkpoint".to_owned(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
            .expect("publish stale checkpoint");
        assert_eq!(
            live.iter().any(|item| item.item == response),
            consume_before_checkpoint,
            "checkpoint publication must not consume pending mailbox context",
        );
        let canonical = store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("canonical history");
        let checkpoint = canonical
            .items
            .iter()
            .rev()
            .find_map(|item| match item {
                RolloutItem::Compacted(checkpoint) => Some(checkpoint.clone()),
                _ => None,
            })
            .expect("checkpoint");
        let canonical_items = checkpoint
            .replacement_history
            .as_ref()
            .expect("replacement");
        assert_eq!(
            canonical_items
                .iter()
                .filter(|item| item.item == response)
                .cloned()
                .collect::<Vec<_>>(),
            vec![ResponseItemEnvelope::new(response.clone())],
        );
        assert_eq!(
            checkpoint.replacement_history_media_sanitized_prefix_len,
            Some(canonical_items.len() as u64),
        );
        // Exercise the core side of the bounded-reader contract: the accepted pair before
        // the selected checkpoint is deliberately absent. Local/Paginated reader coverage
        // separately verifies that it supplies this replacement history.
        let (fresh, fresh_turn, _) = make_session_and_context_with_rx().await;
        let restored = fresh
            .reconstruct_history_from_rollout(&fresh_turn, &[RolloutItem::Compacted(checkpoint)])
            .await;
        assert_eq!(
            restored
                .history
                .iter()
                .filter(|item| item.item == response)
                .cloned()
                .collect::<Vec<_>>(),
            vec![ResponseItemEnvelope::new(response.clone())],
        );
        assert!(restored.should_recompute_token_usage);
        if !consume_before_checkpoint {
            let _ = session.input_queue.drain_mailbox_input_items().await;
            session
                .consume_completion_context(&communication, turn.model_info())
                .await
                .expect("consume after checkpoint");
        }
        assert_eq!(
            session
                .clone_history()
                .await
                .raw_items()
                .filter(|item| *item == &response)
                .cloned()
                .collect::<Vec<_>>(),
            vec![response.clone()],
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .acknowledged_completion_contexts
                .len(),
            1
        );
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let installed = session
            .publish_compacted_history(
                Vec::new(),
                /*reference_context_item*/ None,
                /*world_state_baseline*/ None,
                CompactedHistoryMetadata {
                    completion_source_items: crate::compact::completion_source_items(
                        std::slice::from_ref(&response),
                    ),
                    message: "later summary whose request included the completion".to_owned(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
            .expect("source-covered completion may be summarized");
        assert!(!installed.iter().any(|item| item.item == response));
        assert!(
            session
                .state
                .lock()
                .await
                .acknowledged_completion_contexts
                .is_empty()
        );
    }
}
