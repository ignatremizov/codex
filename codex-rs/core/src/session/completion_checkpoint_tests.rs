use super::*;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum CheckpointKind {
    Complete,
    Prefix,
}

#[test_case(CheckpointKind::Complete; "complete replacement")]
#[test_case(CheckpointKind::Prefix; "prepared prefix with newer media")]
#[tokio::test]
async fn acknowledged_completion_survives_stale_checkpoint_before_or_after_consumption(
    kind: CheckpointKind,
) {
    // A replacement computed before the completion is deliberately reused in both orders.
    for consume_before_checkpoint in [false, true] {
        let (mut session, turn, _) = make_session_and_context_with_rx().await;
        let store =
            attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
        let stale_replacement = Vec::new();
        let prefix = {
            let state = session.state.lock().await;
            CompactionPrefix {
                generation: state.history.history_version(),
                len: state.history.annotated_items().len(),
                auth_owner: session.services.model_client.auth_owner(),
            }
        };
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
        let published = store
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("canonical completion source");
        let source_envelope = published
            .items
            .iter()
            .find_map(|item| match item {
                RolloutItem::ResponseItem(envelope) if envelope.item == response => {
                    Some(envelope.clone())
                }
                _ => None,
            })
            .expect("prepared completion envelope");
        assert_eq!(
            source_envelope,
            codex_history::ResponseItemEnvelope {
                item: response.clone(),
                metadata: Some(codex_history::CodexHarnessMetadata {
                    history_sequence: Some(1),
                    sampled_through_sequence: Some(0),
                    ..Default::default()
                }),
            },
            "canonical completion identity is independent from retained human authorization",
        );
        assert!(
            !session
                .clone_history()
                .await
                .raw_items()
                .any(|item| item == &response)
        );
        session
            .input_queue
            .enqueue_mailbox_communication(
                communication.clone().into(),
                TurnStartOptions::default(),
            )
            .await;
        if consume_before_checkpoint {
            let _ = session.input_queue.drain_mailbox_input_items().await;
            session
                .consume_completion_context(&communication, turn.model_info())
                .await
                .expect("consume");
        }
        let mut suffix_media = matches!(kind, CheckpointKind::Prefix).then(|| {
            ResponseItemEnvelope::new(
                serde_json::from_value(serde_json::json!({
                    "type": "message", "role": "user", "id": "newer-media",
                    "content": [{"type": "input_image", "image_url": "data:image/png;base64,newer"}],
                }))
                .expect("newer unsummarized media"),
            )
        });
        if let Some(media) = &mut suffix_media {
            session.state.lock().await.history.record_annotated_items(
                std::slice::from_mut(media),
                TruncationPolicy::Tokens(1000),
            );
        }
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let installation = match kind {
            CheckpointKind::Complete => {
                CompactedHistoryInstallation::Complete(CompactedHistoryMetadata {
                    input_goal_ids: Default::default(),
                    completion_source_items: Vec::new(),
                    message: "stale model checkpoint".to_owned(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                })
            }
            CheckpointKind::Prefix => CompactedHistoryInstallation::Prefix {
                reset_world_state_sections: Vec::new(),
                prefix,
                prefix_context: Vec::new(),
                initial_context: Vec::new(),
                model_info: turn.model_info(),
                completion_source_items: Vec::new(),
                compaction_response_id: "prepared-before-completion".to_owned(),
                compaction_summary_tokens: None,
                reviewer_compaction_hash: None,
                turn_context: &turn,
                started_item: None,
            },
        };
        let live = session
            .replace_compacted_history(
                stale_replacement,
                turn.to_turn_context_item(),
                WorldStateSnapshot::default(),
                Default::default(),
                installation,
            )
            .await
            .expect("publish stale checkpoint")
            .expect("install stale checkpoint");
        assert_eq!(
            live.iter().any(|item| item.item == response),
            consume_before_checkpoint,
            "checkpoint publication must not consume pending mailbox context",
        );
        assert!(
            session.clone_history().await.input_sequence() >= 1,
            "a hidden canonical completion keeps its sampling identity reserved after replacement"
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
            vec![source_envelope.clone()],
        );
        assert_eq!(
            checkpoint.replacement_history_media_sanitized_prefix_len,
            Some(
                (canonical_items.len()
                    - suffix_media
                        .as_ref()
                        .map_or(0, |_| { 1 + usize::from(consume_before_checkpoint) }))
                    as u64
            ),
        );
        if let Some(media) = &suffix_media {
            assert_eq!(canonical_items.last(), Some(media));
            assert_eq!(live.last(), Some(media));
        }
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
            vec![source_envelope],
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
            .replace_compacted_history(
                Vec::new(),
                turn.to_turn_context_item(),
                WorldStateSnapshot::default(),
                Default::default(),
                CompactedHistoryMetadata {
                    input_goal_ids: Default::default(),
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
            .expect("source-covered completion may be summarized")
            .expect("install source-covered checkpoint");
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

#[tokio::test]
async fn queue_only_completions_reserve_distinct_sampling_identities_without_sampling() {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique")).await;
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let mut communications = Vec::new();
    for text in ["first queued result", "second queued result"] {
        let mut communication = InterAgentCommunication::new(
            codex_protocol::AgentPath::try_from("/root/child").expect("path"),
            codex_protocol::AgentPath::root(),
            Vec::new(),
            text.to_owned(),
            /*trigger_turn*/ false,
        );
        communication.id = Some(new_sub_agent_completion_context_response_item_id());
        session
            .persist_completion_context(
                communication.to_model_input_item(),
                &accepted,
                CompletionContextDelivery::QueueOnly,
            )
            .await
            .expect("canonical queued completion");
        communications.push(communication);
    }
    let pending = session.clone_history().await;
    assert!(pending.annotated_items().is_empty());
    assert_eq!(pending.input_sequence(), 2);
    let ordinary: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "subsequent admitted input"}],
    }))
    .expect("ordinary input");
    session
        .record_conversation_items(&turn, turn.model_info(), &[ordinary])
        .await;
    let published = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        })
        .await
        .expect("canonical history");
    let identities = published
        .items
        .iter()
        .filter_map(|record| match record {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .map(|metadata| (metadata.history_sequence, metadata.sampled_through_sequence)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        identities,
        vec![(Some(1), Some(0)), (Some(2), Some(0)), (Some(3), Some(0))]
    );
    assert_eq!(session.clone_history().await.annotated_items().len(), 1);
    for communication in communications {
        session
            .consume_completion_context(&communication, turn.model_info())
            .await
            .expect("consume already-persisted completion");
    }
    let visible = session.clone_history().await;
    let identities = visible
        .annotated_items()
        .iter()
        .map(|envelope| {
            let metadata = envelope.metadata.as_ref().expect("sampling identity");
            (metadata.history_sequence, metadata.sampled_through_sequence)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        identities,
        vec![(Some(3), Some(0)), (Some(1), Some(0)), (Some(2), Some(0))]
    );
}
