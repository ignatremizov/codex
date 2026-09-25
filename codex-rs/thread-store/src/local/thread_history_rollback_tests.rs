use super::*;
use crate::local::thread_history_materialization;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use codex_protocol::protocol::ThreadRolledBackEvent;
use std::io::Write;

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
        // The table names are fixed fixture constants; the variable values remain bound.
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
