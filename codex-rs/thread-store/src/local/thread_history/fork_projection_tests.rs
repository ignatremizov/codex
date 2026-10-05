use super::ensure_table;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn concurrent_schema_upgrade_preserves_existing_checkpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::TempDir::new()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let database = home.path().join("history.sqlite");
    let first = sqlite.open_read_write_pool(&database).await?;
    let second = sqlite.open_read_write_pool(&database).await?;
    sqlx::query(
        "CREATE TABLE fork_thread_history_projection_state (
            thread_id TEXT PRIMARY KEY,
            next_rollout_byte_offset INTEGER NOT NULL,
            next_rollout_ordinal INTEGER NOT NULL
        )",
    )
    .execute(&first)
    .await?;
    sqlx::query(
        "INSERT INTO fork_thread_history_projection_state VALUES ('existing-thread', 120, 7)",
    )
    .execute(&first)
    .await?;

    // Different pools model independent stores/processes sharing the derived database.
    let upgrades = (0..16).map(|index| {
        let pool = if index % 2 == 0 { &first } else { &second };
        ensure_table(pool)
    });
    for result in futures::future::join_all(upgrades).await {
        result?;
    }
    for pool in [&first, &second] {
        ensure_table(pool).await?;
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            "SELECT thread_id, next_rollout_byte_offset, next_rollout_ordinal, projection_version
             FROM fork_thread_history_projection_state ORDER BY thread_id",
        )
        .fetch_all(pool)
        .await?;
        assert_eq!(rows, vec![("existing-thread".to_owned(), 120, 7, 0)]);
    }
    first.close().await;
    second.close().await;
    Ok(())
}
