//! Legacy child migration preserves audit history without expanding cold-resume model input.

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_thread_store::ItemSortKey;
use codex_thread_store::ListItemsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::RolloutMigrationMode;
use codex_thread_store::RolloutMigrationOptions;
use codex_thread_store::RolloutMigrationStatus;
use codex_thread_store::SortDirection;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::MockServer;

#[tokio::test]
async fn migrated_child_keeps_audit_sentinels_outside_bounded_cold_resume_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Legacy)
        .with_config(|config| {
            config
                .features
                .disable(Feature::BackgroundPaginatedRolloutMigration)
                .expect("manual migration owns this fixture");
            config.model_auto_compact_token_limit = Some(100_000);
        })
        .build_with_auto_env(&server)
        .await?;
    let environments = test.codex.environment_selections().await;
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            history_mode: Some(ThreadHistoryMode::Legacy),
            session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                "migration-test".to_string(),
            ))),
            environments: Some(environments.clone()),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    turn(
        &server,
        &child.thread,
        "OLD_USER_SENTINEL",
        "OLD_ASSISTANT_SENTINEL",
    )
    .await?;

    // Record historical rollback and checkpoints as a legacy fixture. The final live turn below
    // supplies real completed-turn context, including foreign executor workspace metadata.
    child
        .thread
        .append_rollout_items(&[
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "removed-turn".to_string(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "ROLLED_BACK_SENTINEL".to_string(),
                ..UserMessageEvent::default()
            })),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "removed-turn".to_string(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            })),
        ])
        .await?;
    for (window, message) in [(1, "SUPERSEDED_CHECKPOINT"), (2, "LATEST_CHECKPOINT")] {
        child
            .thread
            .append_rollout_items(&[RolloutItem::Compacted(CompactedItem {
                message: message.to_string(),
                replacement_history: Some(vec![
                    ResponseItem::Message {
                        id: None,
                        role: "user".to_string(),
                        content: vec![ContentItem::InputText {
                            text: message.to_string(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }
                    .into(),
                ]),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                compaction_summary_tokens: None,
                window_number: Some(window),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                ..Default::default()
            })])
            .await?;
    }
    turn(
        &server,
        &child.thread,
        "TAIL_USER_SENTINEL",
        "TAIL_ASSISTANT_SENTINEL",
    )
    .await?;
    let rollout_path = child.thread.rollout_path().context("child rollout path")?;
    child.thread.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&child.thread_id).await;
    let local_store = test
        .thread_store
        .as_any()
        .downcast_ref::<LocalThreadStore>()
        .context("local canonical thread store")?;
    let report = local_store
        .migrate_rollouts(RolloutMigrationOptions {
            mode: RolloutMigrationMode::Apply,
            thread_ids: vec![child.thread_id],
            max_mib_per_second: None,
        })
        .await?;
    assert_eq!(
        report
            .outcomes
            .iter()
            .map(|outcome| outcome.status)
            .collect::<Vec<_>>(),
        vec![RolloutMigrationStatus::Migrated]
    );
    let (canonical, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    let canonical_json = serde_json::to_string(&canonical)?;
    for sentinel in [
        "OLD_USER_SENTINEL",
        "OLD_ASSISTANT_SENTINEL",
        "SUPERSEDED_CHECKPOINT",
    ] {
        assert!(
            canonical_json.contains(sentinel),
            "missing canonical {sentinel}"
        );
    }
    assert!(!canonical_json.contains("ROLLED_BACK_SENTINEL"));
    let audit = test
        .thread_store
        .list_items(ListItemsParams {
            thread_id: child.thread_id,
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 100,
            sort_direction: SortDirection::Asc,
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
        })
        .await?;
    let audit_items = audit
        .items
        .iter()
        .map(|item| serde_json::from_slice::<serde_json::Value>(&item.item_json))
        .collect::<Result<Vec<_>, _>>()?;
    let audit_json = serde_json::to_string(&audit_items)?;
    for sentinel in ["OLD_USER_SENTINEL", "OLD_ASSISTANT_SENTINEL"] {
        assert!(
            audit_json.contains(sentinel),
            "missing projected {sentinel}"
        );
    }
    let context = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: child.thread_id,
            include_archived: false,
        })
        .await?;
    let resumed = test
        .thread_manager
        .start_thread(StartThreadOptions {
            history_mode: Some(ThreadHistoryMode::Paginated),
            environments: Some(environments),
            initial_history: InitialHistory::Resumed(ResumedHistory {
                conversation_id: child.thread_id,
                history: Arc::new(context.items),
                rollout_path: Some(rollout_path.clone()),
            }),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    let mock = turn(
        &server,
        &resumed.thread,
        "RESUMED_FOLLOWUP",
        "resumed answer",
    )
    .await?;
    let request = mock.single_request();
    let input = serde_json::to_string(&request.input())?;
    for sentinel in [
        "LATEST_CHECKPOINT",
        "TAIL_USER_SENTINEL",
        "TAIL_ASSISTANT_SENTINEL",
        "RESUMED_FOLLOWUP",
    ] {
        assert!(input.contains(sentinel), "missing model context {sentinel}");
    }
    for sentinel in [
        "OLD_USER_SENTINEL",
        "OLD_ASSISTANT_SENTINEL",
        "SUPERSEDED_CHECKPOINT",
        "ROLLED_BACK_SENTINEL",
    ] {
        assert!(
            !input.contains(sentinel),
            "unexpected model context {sentinel}"
        );
    }
    resumed.thread.shutdown_and_wait().await?;
    let (after_resume, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(
        serde_json::to_value(&after_resume[..canonical.len()])?,
        serde_json::to_value(canonical)?
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

async fn turn(
    server: &MockServer,
    thread: &CodexThread,
    prompt: &str,
    reply: &str,
) -> Result<ResponseMock> {
    let mock = responses::mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_assistant_message("reply", reply),
            responses::ev_completed(prompt),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(mock)
}
