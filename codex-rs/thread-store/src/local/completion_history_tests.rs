use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn bounded_context_retains_checkpoint_completion_once_after_original_pair_cutoff() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 4003);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let context = ResponseItem::AgentMessage {
        id: Some(new_sub_agent_completion_context_response_item_id()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "completed before compaction".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let checkpoint = compacted("retained completion", Some(vec![context.clone()]));
    let latest_turn = [
        turn_started("latest-turn"),
        user_message("continue after compaction"),
        completed_user_message("latest-turn", "continue after compaction"),
        turn_context(home.path(), "latest-turn"),
        turn_complete("latest-turn"),
    ];
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-03-02",
        uuid,
        [
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            RolloutItem::ResponseItem(context.into()),
            checkpoint.clone(),
        ],
    );
    append_items(&path, latest_turn.clone());
    let session_meta = codex_rollout::read_session_meta_line(&path)
        .await
        .expect("read session metadata");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let bounded = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load bounded model context");

    // Only the checkpoint carries the completion: its original adjacent metadata/context
    // pair lies before the bounded reader's cutoff, and must not be replayed a second time.
    let mut expected = vec![RolloutItem::SessionMeta(session_meta), checkpoint];
    expected.extend(latest_turn);
    assert_eq!(
        serde_json::to_value(bounded.items).expect("serialize bounded context"),
        serde_json::to_value(expected).expect("serialize expected context"),
    );
}

#[tokio::test]
async fn completion_lookup_masks_each_frozen_compressed_source_before_concatenating() {
    let home = TempDir::new().expect("temp dir");
    let root_uuid = Uuid::from_u128(/*v*/ 4001);
    let root_id = ThreadId::from_string(&root_uuid.to_string()).expect("root id");
    let child_uuid = Uuid::from_u128(/*v*/ 4002);
    let child_id = ThreadId::from_string(&child_uuid.to_string()).expect("child id");
    let context_id = new_sub_agent_completion_context_response_item_id();
    let context = ResponseItem::AgentMessage {
        id: Some(context_id.clone()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "done".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let event = ItemCompletedEvent {
        thread_id: root_id,
        turn_id: "root-completion".to_string(),
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
    let excluded = ItemCompletedEvent {
        item: TurnItem::AgentMessage(
            sub_agent_completion_item("/root/later", &AgentStatus::Shutdown).expect("terminal"),
        ),
        ..event.clone()
    };
    let rollback = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        codex_protocol::protocol::ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: Some(0),
        },
    ));
    let root_path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-03-00",
        root_uuid,
        [
            turn_started("root-completion"),
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            RolloutItem::ResponseItem(context.clone().into()),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event.clone())),
            turn_complete("root-completion"),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(excluded.clone())),
            rollback.clone(),
        ],
    );
    let frozen = history_position(&root_path, root_id, /*end_ordinal_exclusive*/ 6);
    let child_path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-03-01",
        child_uuid,
        [user_message("removed child prefix"), rollback],
    );
    set_history_base(&child_path, frozen);
    let archived_dir = home.path().join("archived_sessions");
    std::fs::create_dir_all(&archived_dir).expect("archive directory");
    let archived = archived_dir
        .join(root_path.file_name().expect("rollout filename"))
        .with_extension("jsonl.zst");
    let bytes = std::fs::read(&root_path).expect("read canonical source");
    std::fs::write(
        &archived,
        zstd::stream::encode_all(bytes.as_slice(), /*level*/ 0).expect("compress source"),
    )
    .expect("write archive");
    std::fs::remove_file(&root_path).expect("retire plain source");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert_eq!(
        store
            .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                thread_id: child_id,
                include_archived: false,
                response_item_id: context_id,
            })
            .await
            .expect("inherited context"),
        Some(context),
    );
    let found = store
        .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
            thread_id: child_id,
            include_archived: false,
            item_id: event.item.id(),
            turn_id: event.turn_id.clone(),
        })
        .await
        .expect("inherited presentation");
    assert_eq!(
        (
            serde_json::to_value(found.item_completed).expect("actual"),
            found.turn_started,
            found.turn_completed
        ),
        (
            serde_json::to_value(Some(event)).expect("expected"),
            true,
            true
        ),
    );
    let outside = store
        .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
            thread_id: child_id,
            include_archived: false,
            item_id: excluded.item.id(),
            turn_id: excluded.turn_id,
        })
        .await
        .expect("outside frozen cutoff");
    assert!(outside.item_completed.is_none());
}
