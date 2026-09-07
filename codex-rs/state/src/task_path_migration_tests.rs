use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn task_path_migration_upgrades_existing_aliases_without_reusing_lifecycle_paths() {
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .expect("database");
    migrator_through(/*version*/ 10_050)
        .run(&mut connection)
        .await
        .expect("actual pre-task-label schema");
    sqlx::query(
        "INSERT INTO agent_alias_namespaces (session_id, next_agent_ref) VALUES ('root', 3)",
    )
    .execute(&mut connection)
    .await
    .expect("existing namespace");
    sqlx::query(
        "INSERT INTO agent_aliases \
         (session_id, thread_id, agent_ref, nickname, ownership_state) \
         VALUES ('root', 'root', 1, 'Main', 'current'), \
                ('root', 'child', 2, 'Coder', 'current')",
    )
    .execute(&mut connection)
    .await
    .expect("existing aliases");
    sqlx::query(
        r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, created_at_ms, updated_at_ms,
    source, model_provider, cwd, title, sandbox_policy, approval_mode, agent_path
) VALUES (
    'child', '/tmp/existing.jsonl', 1, 1, 1000, 1000,
    'cli', 'openai', '/tmp', '', 'read-only', 'on-request', '/root/lifecycle-child'
)
        "#,
    )
    .execute(&mut connection)
    .await
    .expect("existing lifecycle metadata");

    STATE_MIGRATOR
        .run(&mut connection)
        .await
        .expect("append-only task label migration");
    assert_eq!(
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT thread_id, task_path FROM agent_aliases ORDER BY agent_ref",
        )
        .fetch_all(&mut connection)
        .await
        .expect("migrated aliases"),
        vec![
            ("root".to_string(), Some("/root".to_string())),
            ("child".to_string(), None),
        ],
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT agent_path FROM threads WHERE id = 'child'")
            .fetch_one(&mut connection)
            .await
            .expect("lifecycle metadata"),
        "/root/lifecycle-child",
    );
    sqlx::query("UPDATE agent_aliases SET task_path = '/root/backend' WHERE thread_id = 'child'")
        .execute(&mut connection)
        .await
        .expect("assign label");
    sqlx::query(
        "INSERT INTO agent_aliases \
         (session_id, thread_id, agent_ref, ownership_state, task_path) \
         VALUES ('root', 'another', 3, 'current', '/root/backend')",
    )
    .execute(&mut connection)
    .await
    .expect_err("unique index independently rejects another current owner");
    sqlx::query(
        "UPDATE agent_aliases SET ownership_state = 'transferred' WHERE thread_id = 'child'",
    )
    .execute(&mut connection)
    .await
    .expect("historical owner");
    sqlx::query(
        "INSERT INTO agent_aliases \
         (session_id, thread_id, agent_ref, ownership_state, task_path) \
         VALUES ('root', 'another', 3, 'current', '/root/backend')",
    )
    .execute(&mut connection)
    .await
    .expect("historical assignment does not reserve the path");
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT task_path FROM agent_aliases WHERE thread_id = 'child'",
        )
        .fetch_one(&mut connection)
        .await
        .expect("retained historical task path"),
        Some("/root/backend".to_string()),
    );
}
