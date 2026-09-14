//! The lower-numbered attachment migration upgrades an already released fork database.

use super::*;
use crate::migrations::STATE_MIGRATOR;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

#[tokio::test]
async fn attachment_upgrade_preserves_later_fork_migrations_and_durable_identity()
-> anyhow::Result<()> {
    let home = unique_temp_dir();
    let _cleanup = scopeguard::guard(home.clone(), |home| {
        let _ = std::fs::remove_dir_all(home);
    });
    tokio::fs::create_dir_all(&home).await?;
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let old_migrator = Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version != 62)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
    };
    let pool = config.open_read_write_pool(&config.state_db_path()).await?;
    old_migrator.run(&pool).await?;
    let parent = ThreadId::new();
    let child = ThreadId::new();
    let deleted = ThreadId::new();
    sqlx::query(
        "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, \
         model_provider, cwd, title, sandbox_policy, approval_mode, service_tier) \
         VALUES (?, 'saved.jsonl', 1, 1, 'cli', 'test-provider', '.', \
         'Retained title', 'danger-full-access', 'never', 'fast')",
    )
    .bind(parent.to_string())
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO agent_alias_namespaces (session_id, next_agent_ref) VALUES (?, 3)")
        .bind(parent.to_string())
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO agent_aliases (session_id, thread_id, agent_ref, nickname, ownership_state, task_path) \
         VALUES (?, ?, 2, 'worker', 'current', '/root/worker')",
    ).bind(parent.to_string()).bind(child.to_string()).execute(&pool).await?;
    sqlx::query(
        "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) \
         VALUES (?, ?, 'closed')",
    )
    .bind(parent.to_string())
    .bind(child.to_string())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_thread_lifecycle_authority_epochs (thread_id, epoch) VALUES (?, 7)",
    )
    .bind(child.to_string())
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO agent_alias_tombstones (thread_id) VALUES (?)")
        .bind(deleted.to_string())
        .execute(&pool)
        .await?;
    let applied = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await?;
    assert!(applied.iter().any(|(version, _)| *version == 10_054));
    pool.close().await;

    let runtime = StateRuntime::init(config, "test-provider".to_string()).await?;
    // A second pass must validate the same checksums without applying 0062 again.
    runtime_state_migrator().run(runtime.pool.as_ref()).await?;
    let upgraded = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(runtime.pool.as_ref())
    .await?;
    assert_eq!(
        upgraded
            .iter()
            .filter(|(version, _)| *version == 62)
            .count(),
        1
    );
    assert_eq!(
        upgraded
            .into_iter()
            .filter(|(version, _)| *version != 62)
            .collect::<Vec<_>>(),
        applied
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, i64, String, String)>(
            "SELECT session_id, thread_id, agent_ref, nickname, task_path FROM agent_aliases",
        )
        .fetch_all(runtime.pool.as_ref())
        .await?,
        vec![(
            parent.to_string(),
            child.to_string(),
            2,
            "worker".into(),
            "/root/worker".into()
        )]
    );
    assert_eq!(
        runtime.read_agent_thread_lifecycle_epochs(&[child]).await?,
        vec![(child, 7)]
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT parent_thread_id, child_thread_id, status FROM thread_spawn_edges",
        )
        .fetch_all(runtime.pool.as_ref())
        .await?,
        vec![(parent.to_string(), child.to_string(), "closed".into())]
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT thread_id FROM agent_alias_tombstones")
            .fetch_all(runtime.pool.as_ref())
            .await?,
        vec![deleted.to_string()]
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT title, service_tier, attached_thread_parent_id FROM threads WHERE id = ?",
        )
        .bind(parent.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await?,
        ("Retained title".into(), "fast".into(), None)
    );
    let attached = ThreadId::new();
    let mut metadata = test_thread_metadata(&home, attached, home.clone());
    metadata.attached_thread_parent_id = Some(parent);
    runtime.upsert_thread(&metadata).await?;
    assert_eq!(
        runtime
            .get_thread(attached)
            .await?
            .and_then(|thread| thread.attached_thread_parent_id),
        Some(parent)
    );
    Ok(())
}
