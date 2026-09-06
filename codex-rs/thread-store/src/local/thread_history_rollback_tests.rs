use super::*;
use crate::local::thread_history_materialization;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use codex_protocol::protocol::ThreadRolledBackEvent;
use std::io::Write;

#[test]
fn exact_projection_matches_canonical_lifecycle_and_catalog_mask_without_payload_retention() {
    let thread_id = ThreadId::new();
    let items = vec![
        turn_started("retained"),
        completed_item(
            thread_id,
            "retained",
            agent_message("ordinary", MessagePhase::FinalAnswer),
        ),
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        user_message("ordinary body is not delivery proof"),
        RolloutItem::DynamicToolsUpdated {
            thread_id,
            tools: Vec::new(),
        },
        turn_completed("retained"),
        turn_started("removed"),
        user_message("removed body"),
        turn_completed("removed"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: Some(1),
        })),
    ];
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for item in &items {
        offsets.push(u64::try_from(bytes.len()).expect("source offset"));
        serde_json::to_writer(
            &mut bytes,
            &RolloutLine {
                timestamp: "2026-10-25T00:00:00Z".to_string(),
                ordinal: None,
                item: item.clone(),
            },
        )
        .expect("encode canonical record");
        bytes.push(b'\n');
    }
    let canonical_mask = codex_rollout::exact_rollback_removed_items(&items);
    assert_eq!(
        canonical_mask,
        vec![
            false, true, true, true, false, false, true, true, true, true
        ]
    );
    let projection = super::super::rollback::ExactRollbackProjection::read(
        &mut std::io::Cursor::new(bytes),
        /*start_offset*/ 0,
        || false,
    )
    .expect("exact source projection");
    let expected = offsets
        .into_iter()
        .zip(canonical_mask)
        .filter_map(|(offset, removed)| removed.then_some(offset))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        (projection.contains_exact_marker, projection.removed_offsets),
        (true, expected)
    );
}

#[tokio::test]
async fn exact_rollback_rebuilds_paginated_projection_without_removed_turn() {
    let home = TempDir::new().expect("temp dir");
    let store = projection_store(home.path()).await;
    let thread_id = ThreadId::default();
    create_paginated_thread(&store, thread_id).await;
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist session metadata");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![
                turn_started("retained-turn"),
                completed_item(
                    thread_id,
                    "retained-turn",
                    agent_message("retained-agent", MessagePhase::FinalAnswer),
                ),
                turn_completed("retained-turn"),
                turn_started("removed-turn"),
                completed_item(
                    thread_id,
                    "removed-turn",
                    agent_message("removed-agent", MessagePhase::FinalAnswer),
                ),
                turn_completed("removed-turn"),
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                    num_turns: 0,
                    materialized_turns: Some(1),
                    rollback_start_index: Some(4),
                })),
            ],
        })
        .await
        .expect("append history and exact rollback");

    let pool = codex_state::open_thread_history_db(&codex_state::SqliteConfig::new_for_testing(
        home.path().abs(),
    ))
    .await
    .expect("open thread history db");
    let turns = sqlx::query_as::<_, (String, String)>(
        "SELECT turn_id, status FROM thread_turns WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind(thread_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("read rebuilt turns");
    let items = sqlx::query_scalar::<_, String>(
        "SELECT item_id FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind(thread_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("read rebuilt items");
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("live rollout path");
    let rollout_len = i64::try_from(
        fs::metadata(rollout_path.as_path())
            .expect("rollout metadata")
            .len(),
    )
    .expect("rollout length");

    assert_eq!(
        (turns, items, projection_state(&pool, thread_id).await),
        (
            vec![("retained-turn".to_string(), "completed".to_string())],
            vec!["retained-agent".to_string()],
            (rollout_len, 8),
        )
    );
}

#[tokio::test]
async fn exact_projection_uses_decoded_indexes_and_rebuilds_old_eof_checkpoints() {
    let home = TempDir::new().expect("temp dir");
    let store = projection_store(home.path()).await;
    let thread_id = ThreadId::new();
    create_paginated_thread(&store, thread_id).await;
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![
                turn_started("retained"),
                completed_item(
                    thread_id,
                    "retained",
                    agent_message("opaque-retained", MessagePhase::FinalAnswer),
                ),
                turn_completed("retained"),
                turn_started("removed"),
                completed_item(
                    thread_id,
                    "removed",
                    agent_message("opaque-removed", MessagePhase::FinalAnswer),
                ),
                turn_completed("removed"),
            ],
        })
        .await
        .expect("append initial projection");
    let rollout_path = store.live_rollout_path(thread_id).await.expect("path");
    store.shutdown_thread(thread_id).await.expect("shutdown");
    let pool = codex_state::open_thread_history_db(&codex_state::SqliteConfig::new_for_testing(
        home.path().abs(),
    ))
    .await
    .expect("pool");
    let mut lines = fs::read_to_string(&rollout_path)
        .expect("source")
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    // Two decoded records without usable projection ordinals still occupy canonical indexes.
    let extra = serde_json::to_string(&RolloutLine {
        timestamp: "not-a-timestamp".to_string(),
        ordinal: None,
        item: user_message("coordinate-only record"),
    })
    .expect("extra record");
    lines.splice(
        4..4,
        [
            "{malformed".to_string(),
            r#"{"type":"future_record","ordinal":999}"#.to_string(),
            extra.clone(),
            extra,
        ],
    );
    let marker = RolloutLine {
        timestamp: "2026-07-09T00:00:09Z".to_string(),
        ordinal: Some(10_000_000),
        item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(6),
        })),
    };
    lines.push(serde_json::to_string(&marker).expect("marker"));
    let source = format!("{}\n", lines.join("\n"));
    fs::write(&rollout_path, &source).expect("replace canonical fixture");
    let length = i64::try_from(source.len()).expect("length");
    for table in [
        "thread_history_projection_state",
        "fork_thread_history_projection_state",
    ] {
        // Both names are fixed fixture constants; data values remain bound parameters.
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {table} SET next_rollout_byte_offset = ?, next_rollout_ordinal = 10000001 WHERE thread_id = ?"
        ))).bind(length).bind(thread_id.to_string()).execute(&pool).await.expect("old EOF checkpoint");
    }
    sqlx::query("ALTER TABLE fork_thread_history_projection_state DROP COLUMN projection_version")
        .execute(&pool)
        .await
        .expect("old unversioned projection schema");
    thread_history_materialization::materialize_to_sqlite(&store, thread_id, &rollout_path)
        .await
        .expect("rebuild old projection");
    let items: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind(thread_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("items");
    let fork: (i64, i64, i64) = sqlx::query_as(
        "SELECT next_rollout_byte_offset, next_rollout_ordinal, projection_version FROM fork_thread_history_projection_state WHERE thread_id = ?"
    ).bind(thread_id.to_string()).fetch_one(&pool).await.expect("fork checkpoint");
    assert_eq!(
        (items, projection_state(&pool, thread_id).await, fork),
        (
            vec!["opaque-retained".to_string()],
            (length, 10_000_001),
            (length, 10_000_001, 1)
        ),
    );
    assert_eq!(
        fs::read_to_string(&rollout_path).expect("canonical remains intact"),
        source
    );
    sqlx::query("CREATE TRIGGER reject_unnecessary_rebuild BEFORE INSERT ON thread_history_projection_state BEGIN SELECT RAISE(ABORT, 'unexpected second rebuild'); END")
        .execute(&pool).await.expect("forbid another rebuild");
    thread_history_materialization::materialize_to_sqlite(&store, thread_id, &rollout_path)
        .await
        .expect("versioned matching EOF requires no rebuild");
}

#[tokio::test]
async fn failed_exact_rebuild_keeps_old_rows_and_both_checkpoints() {
    let fixture = populated_projection().await;
    fixture
        .store
        .shutdown_thread(fixture.thread_id)
        .await
        .expect("shutdown");
    let before = projection_rows(&fixture.pool, fixture.thread_id).await;
    sqlx::query("UPDATE fork_thread_history_projection_state SET projection_version = 0 WHERE thread_id = ?")
        .bind(fixture.thread_id.to_string()).execute(&fixture.pool).await.expect("uncertified projection");
    let fork_before: (i64, i64, i64) = sqlx::query_as(
        "SELECT next_rollout_byte_offset, next_rollout_ordinal, projection_version FROM fork_thread_history_projection_state WHERE thread_id = ?"
    ).bind(fixture.thread_id.to_string()).fetch_one(&fixture.pool).await.expect("fork checkpoint");
    let path = fixture.rollout_path.clone();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("source");
    let marker = RolloutLine {
        timestamp: "2026-07-09T00:00:09Z".to_string(),
        ordinal: Some(u64::try_from(fork_before.1).expect("ordinal")),
        item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(1),
        })),
    };
    writeln!(file, "{}", serde_json::to_string(&marker).expect("marker")).expect("append");
    drop(file);
    sqlx::query("CREATE TRIGGER reject_exact_projection BEFORE INSERT ON fork_thread_history_projection_state BEGIN SELECT RAISE(ABORT, 'injected checkpoint failure'); END")
        .execute(&fixture.pool).await.expect("trigger");
    assert!(
        thread_history_materialization::materialize_to_sqlite(
            &fixture.store,
            fixture.thread_id,
            &path
        )
        .await
        .is_err()
    );
    let fork_after: (i64, i64, i64) = sqlx::query_as(
        "SELECT next_rollout_byte_offset, next_rollout_ordinal, projection_version FROM fork_thread_history_projection_state WHERE thread_id = ?"
    ).bind(fixture.thread_id.to_string()).fetch_one(&fixture.pool).await.expect("fork checkpoint");
    assert_eq!(
        (
            projection_rows(&fixture.pool, fixture.thread_id).await,
            fork_after
        ),
        (before, fork_before),
    );
}

#[tokio::test]
async fn incremental_exact_rollback_rebuild_keeps_the_captured_ancestor_prefix() {
    for compressed in [false, true] {
        let fixture = populated_projection().await;
        let (start_offset, start_ordinal) =
            projection_state(&fixture.pool, fixture.thread_id).await;
        fixture
            .store
            .append_items(AppendThreadItemsParams {
                thread_id: fixture.thread_id,
                items: vec![
                    turn_started("removed-turn"),
                    completed_item(
                        fixture.thread_id,
                        "removed-turn",
                        agent_message("removed-agent", MessagePhase::FinalAnswer),
                    ),
                    turn_completed("removed-turn"),
                    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                        num_turns: 0,
                        materialized_turns: Some(1),
                        rollback_start_index: Some(5),
                    })),
                ],
            })
            .await
            .expect("canonical exact rollback fixture");
        let expected = projection_rows(&fixture.pool, fixture.thread_id).await;
        let base = prepare_paginated_fork(&fixture.store, fixture.thread_id, ForkBoundary::Latest)
            .await
            .history_base
            .expect("captured rollback prefix");
        let child_id = ThreadId::default();
        create_paginated_subagent_thread(&fixture.store, child_id, Some(base), None).await;
        fixture
            .store
            .persist_thread(child_id, PersistContext::Standard)
            .await
            .expect("persist child reference");
        let reads = fixture
            .store
            .resolve_rollout_lineage(child_id, None)
            .await
            .expect("capture ancestor reads")
            .reads;
        assert_eq!(reads.cutoff(&fixture.rollout_path), Some(base));
        fixture
            .store
            .shutdown_thread(fixture.thread_id)
            .await
            .expect("close parent writer");
        let late = [
            rollout_line(Some(9), turn_started("late-turn")),
            rollout_line(
                Some(10),
                completed_item(
                    fixture.thread_id,
                    "late-turn",
                    agent_message("not-inherited", MessagePhase::FinalAnswer),
                ),
            ),
            rollout_line(Some(11), turn_completed("late-turn")),
        ]
        .join("\n");
        append_suffix(&fixture.rollout_path, &format!("{late}\n"));
        if compressed {
            compress_rollout(&fixture.rollout_path);
        }
        let physical = if compressed {
            fixture.rollout_path.with_extension("jsonl.zst")
        } else {
            fixture.rollout_path.clone()
        };
        let canonical = fs::read(&physical).expect("canonical audit bytes");
        // Simulate an interrupted incremental projection before the exact marker. Both
        // compatibility checkpoints agree, so only the reader outcome can request rebuild.
        for table in [
            "thread_history_projection_state",
            "fork_thread_history_projection_state",
        ] {
            sqlx::query(AssertSqlSafe(format!(
                "UPDATE {table} SET next_rollout_byte_offset = ?, next_rollout_ordinal = ? WHERE thread_id = ?"
            ))).bind(start_offset).bind(start_ordinal).bind(fixture.thread_id.to_string())
                .execute(&fixture.pool).await.expect("restore pre-marker cursor");
        }
        thread_history_materialization::materialize_to_sqlite_with_reads(
            &fixture.store,
            fixture.thread_id,
            &fixture.rollout_path,
            &reads,
        )
        .await
        .expect("bounded exact marker requests guarded rebuild");
        assert_eq!(
            projection_rows(&fixture.pool, fixture.thread_id).await,
            expected
        );
        assert_eq!(
            fs::read(&physical).expect("unchanged canonical bytes"),
            canonical
        );
        if compressed {
            assert!(!fixture.rollout_path.exists());
        }
    }
}

#[test]
fn exact_projection_preserves_deliveries_without_manufacturing_metadata_adjacency() {
    let completion_context = || {
        let id = codex_protocol::protocol::new_sub_agent_completion_context_response_item_id();
        let item = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": id,
            "role": "user",
            "content": [{"type": "input_text", "text": "completion context"}],
        }))
        .expect("completion context fixture");
        RolloutItem::ResponseItem(codex_history::ResponseItemEnvelope::new(item))
    };
    let thread_id = ThreadId::new();
    let completion = codex_protocol::protocol::sub_agent_completion_item(
        "/root/worker",
        &codex_protocol::protocol::AgentStatus::Shutdown,
    )
    .expect("terminal completion");
    let items = vec![
        turn_started("retained"),
        // This ordinary event must not turn into a delivery-metadata placeholder.
        completed_item(
            thread_id,
            "retained",
            agent_message("ordinary", MessagePhase::FinalAnswer),
        ),
        completion_context(),
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        completion_context(),
        completed_item(thread_id, "retained", TurnItem::AgentMessage(completion)),
        turn_completed("retained"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: Some(1),
        })),
    ];
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for item in &items {
        offsets.push(u64::try_from(bytes.len()).expect("source offset"));
        serde_json::to_writer(
            &mut bytes,
            &RolloutLine {
                timestamp: "2026-10-25T00:00:00Z".to_string(),
                ordinal: None,
                item: item.clone(),
            },
        )
        .expect("encode source record");
        bytes.push(b'\n');
    }
    let projection = super::super::rollback::ExactRollbackProjection::read(
        &mut std::io::Cursor::new(bytes),
        /*start_offset*/ 0,
        || false,
    )
    .expect("exact source projection");
    let expected = offsets
        .into_iter()
        .enumerate()
        .filter_map(|(index, offset)| matches!(index, 1 | 2 | 7).then_some(offset))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        (projection.contains_exact_marker, projection.removed_offsets),
        (true, expected),
    );
}

#[test]
fn exact_projection_preserves_only_adjacent_committed_observations() {
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::protocol::AgentResponseFinalDelivery;
    use codex_protocol::protocol::AgentResponseObservation;

    for (gap_before, gap_after) in [(false, false), (true, false), (false, true)] {
        for committed in [false, true] {
            let id = codex_protocol::ResponseItemId::new("amsg");
            let response = RolloutItem::ResponseItem(
                codex_protocol::models::ResponseItem::AgentMessage {
                    id: Some(id.clone()),
                    author: "/root/worker".into(),
                    recipient: "/root".into(),
                    content: vec![AgentMessageInputContent::InputText {
                        text: "observed answer".into(),
                    }],
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            );
            let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
                reply_route_enabled: None,
                reply_route_context_installed: false,
                observer_thread_id: ThreadId::new(),
                target_thread_id: ThreadId::new(),
                target_turn_id: Some("target-turn".into()),
                task_preview: None,
                promoted_task_context: None,
                pending_commentary: false,
                commentary_after_sequences: Vec::new(),
                commentary_admissions: Vec::new(),
                commentary_delivery: None,
                target_messages: false,
                queue_delivery: false,
                message_wake_turn_id: None,
                baseline_final_delivery: AgentResponseFinalDelivery::Passive,
                final_delivery: AgentResponseFinalDelivery::Wake,
                final_delivery_response_item_id: Some(id.clone()),
                committed_delivery_response_item_ids: if committed { vec![id] } else { Vec::new() },
            });
            let mut items = vec![
                turn_started("retained"),
                RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true },
            ];
            if gap_before {
                items.push(user_message("must not become metadata"));
            }
            let response_index = items.len();
            items.push(response);
            if gap_after {
                items.push(user_message("must not become an observation"));
            }
            let observation_index = items.len();
            items.push(observation);
            let terminal_index = items.len();
            items.push(turn_completed("retained"));
            items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent {
                    num_turns: 1,
                    materialized_turns: None,
                    rollback_start_index: Some(1),
                },
            )));
            let mut bytes = Vec::new();
            let mut offsets = Vec::new();
            for item in &items {
                offsets.push(u64::try_from(bytes.len()).expect("record offset"));
                serde_json::to_writer(
                    &mut bytes,
                    &RolloutLine {
                        timestamp: "2026-11-15T00:00:00Z".into(),
                        ordinal: None,
                        item: item.clone(),
                    },
                )
                .expect("canonical record");
                bytes.push(b'\n');
            }
            let projection = super::super::rollback::ExactRollbackProjection::read(
                &mut std::io::Cursor::new(bytes),
                /*start_offset*/ 0,
                || false,
            )
            .expect("exact projection");
            let trusted = committed && !gap_before && !gap_after;
            let expected = offsets
                .into_iter()
                .enumerate()
                .filter_map(|(index, offset)| {
                    let keep = index == 0
                        || index == observation_index
                        || index == terminal_index
                        || (trusted && (index == 1 || index == response_index));
                    (!keep).then_some(offset)
                })
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(
                (projection.contains_exact_marker, projection.removed_offsets),
                (true, expected)
            );
        }
    }
}

#[test]
fn exact_projection_checks_promoted_task_proofs_and_conflicting_checkpoints() {
    use codex_protocol::items::UserAgentControlAction;
    use codex_protocol::items::UserAgentControlItem;
    use codex_protocol::protocol::AgentResponseFinalDelivery;
    use codex_protocol::protocol::AgentResponseObservation;
    use codex_protocol::protocol::AgentResponsePromotedTaskContext;
    use codex_protocol::protocol::new_user_agent_task_context_response_item_id;

    for (separated, conflicting_checkpoint) in [(false, false), (true, false), (false, true)] {
        let task = codex_history::ResponseItemEnvelope::new(
            serde_json::from_value(serde_json::json!({
                "type": "message", "role": "user",
                "id": new_user_agent_task_context_response_item_id(),
                "content": [{"type": "input_text", "text": "<user_agent_task>review</user_agent_task>"}],
            })).expect("user task"),
        );
        let promoted = AgentResponsePromotedTaskContext::from_response_item(&task.item)
            .expect("task DTO shape");
        let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
            reply_route_enabled: None,
            reply_route_context_installed: false,
            observer_thread_id: ThreadId::new(),
            target_thread_id: ThreadId::new(),
            target_turn_id: Some("target-turn".into()),
            task_preview: None,
            promoted_task_context: Some(promoted),
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
        });
        let mut checkpoint_task = task.clone();
        if conflicting_checkpoint {
            let codex_protocol::models::ResponseItem::Message { content, .. } =
                &mut checkpoint_task.item
            else {
                unreachable!();
            };
            *content = vec![codex_protocol::models::ContentItem::InputText {
                text: "<user_agent_task>different</user_agent_task>".into(),
            }];
        }
        let thread_id = ThreadId::new();
        let mut items = vec![
            turn_started("retained"),
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            RolloutItem::ResponseItem(task),
        ];
        if separated {
            items.push(user_message("not promotion evidence"));
        }
        let observation_index = items.len();
        items.push(observation);
        items.push(RolloutItem::Compacted(Box::new(
            codex_history::CompactedItem {
                replacement_history: Some(vec![checkpoint_task]),
                ..Default::default()
            },
        )));
        let control_index = items.len();
        items.push(completed_item(
            thread_id,
            "retained",
            TurnItem::UserAgentControl(UserAgentControlItem::succeeded(
                UserAgentControlAction::Prompt,
            )),
        ));
        let terminal_index = items.len();
        items.push(turn_completed("retained"));
        items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: Some(1),
            },
        )));
        let mut bytes = Vec::new();
        let mut offsets = Vec::new();
        for item in &items {
            offsets.push(u64::try_from(bytes.len()).expect("record offset"));
            serde_json::to_writer(
                &mut bytes,
                &RolloutLine {
                    timestamp: "2026-11-15T00:00:00Z".into(),
                    ordinal: None,
                    item: item.clone(),
                },
            )
            .expect("canonical record");
            bytes.push(b'\n');
        }
        let projection = super::super::rollback::ExactRollbackProjection::read(
            &mut std::io::Cursor::new(bytes),
            /*start_offset*/ 0,
            || false,
        )
        .expect("exact projection");
        let trusted = !separated && !conflicting_checkpoint;
        let expected = offsets
            .into_iter()
            .enumerate()
            .filter_map(|(index, offset)| {
                let keep = index == 0
                    || index == observation_index
                    || index == control_index
                    || index == terminal_index
                    || (trusted && (index == 1 || index == 2));
                (!keep).then_some(offset)
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            (projection.contains_exact_marker, projection.removed_offsets),
            (true, expected)
        );
    }
}

#[test]
fn exact_projection_retains_only_classified_non_client_reply_route_context() {
    for (client_authored, classified, mixed) in [
        (false, true, false),  // Harness singleton.
        (true, true, false),   // Client-authored lookalike.
        (false, false, false), // Unclassified lookalike.
        (false, true, true),   // Mixed prompt must not be promoted.
    ] {
        let source = ThreadId::new();
        let mut content = vec![codex_protocol::models::ContentItem::InputText {
            text: format!(
                "<agent_reply_route>\n{{\"agent_id\":\"{source}\",\"send_input\":\"allowed_until_disabled\"}}\n</agent_reply_route>"
            ),
        }];
        if mixed {
            content.push(codex_protocol::models::ContentItem::InputText {
                text: "ordinary user instructions must not be pinned".to_string(),
            });
        }
        let route = codex_history::ResponseItemEnvelope {
            item: codex_protocol::models::ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content,
                status: None,
                phase: None,
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: Some(
                    codex_protocol::models::InternalChatMessageMetadataPassthrough {
                        content_item_kinds: classified.then(|| {
                            vec![codex_protocol::models::ContentItemKind(
                                codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND
                                    .to_string(),
                            )]
                        }),
                        ..Default::default()
                    },
                ),
            },
            metadata: Some(codex_history::CodexHarnessMetadata {
                client_authored,
                ..Default::default()
            }),
        };
        let trusted = classified && !client_authored && !mixed;
        assert_eq!(
            codex_history::persistent_agent_reply_route_source(&route),
            trusted.then_some(source)
        );
        let items = vec![
            turn_started("kept"),
            user_message("kept prompt"),
            RolloutItem::ResponseItem(route),
            user_message("removed prompt"),
            turn_completed("kept"),
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: Some(2),
            })),
        ];
        let mask = codex_rollout::exact_rollback_removed_items(&items);
        assert_eq!(mask[2], !trusted);
        assert!(mask[3]);
        let mut bytes = Vec::new();
        let mut offsets = Vec::new();
        for item in &items {
            offsets.push(u64::try_from(bytes.len()).expect("record offset"));
            serde_json::to_writer(
                &mut bytes,
                &RolloutLine {
                    timestamp: "2026-11-15T00:00:00Z".to_string(),
                    ordinal: None,
                    item: item.clone(),
                },
            )
            .expect("encode canonical record");
            bytes.push(b'\n');
        }
        let projection = super::super::rollback::ExactRollbackProjection::read(
            &mut std::io::Cursor::new(bytes),
            /*start_offset*/ 0,
            || false,
        )
        .expect("exact projection");
        assert_eq!(
            (projection.contains_exact_marker, projection.removed_offsets),
            (
                true,
                offsets
                    .into_iter()
                    .zip(mask)
                    .filter_map(|(offset, removed)| removed.then_some(offset))
                    .collect(),
            )
        );
    }
}
