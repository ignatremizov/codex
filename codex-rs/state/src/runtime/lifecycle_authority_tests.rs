use super::*;
use crate::AgentAliasAllocation;
use crate::AgentAliasTransferRequest;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::SqliteConfig;
use crate::migrations::STATE_MIGRATOR;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

#[tokio::test]
async fn restoration_close_requires_exact_captured_owner_and_parent_after_adoption() {
    let home = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap();
    let root = ThreadId::new();
    let destination = ThreadId::new();
    let child = ThreadId::new();
    runtime
        .allocate_agent_alias(AgentAliasAllocation {
            session_id: root.into(),
            parent_thread_id: root,
            child_thread_id: child,
            nickname: None,
            task_path: None,
        })
        .await
        .unwrap();
    let previous = ThreadSpawnEdgeAuthority {
        thread_id: child,
        parent_thread_id: root,
        owner_session_id: Some(root.into()),
    };
    runtime
        .transfer_agent_alias(AgentAliasTransferRequest {
            expected_previous_session_id: Some(root.into()),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: child,
            nickname: None,
            authored_selector: child.to_string(),
            task_path: None,
        })
        .await
        .unwrap();
    let aliases = runtime
        .list_agent_aliases(destination.into())
        .await
        .unwrap();
    for stale in [
        previous,
        ThreadSpawnEdgeAuthority {
            owner_session_id: Some(destination.into()),
            ..previous
        },
        ThreadSpawnEdgeAuthority {
            parent_thread_id: destination,
            ..previous
        },
    ] {
        assert!(
            runtime
                .close_thread_spawn_edge_if_current(stale, &[child])
                .await
                .is_err()
        );
        assert_eq!(
            runtime
                .list_agent_aliases(destination.into())
                .await
                .unwrap(),
            aliases,
        );
        assert_eq!(
            runtime
                .read_agent_thread_lifecycle_epochs(&[child])
                .await
                .unwrap(),
            vec![(child, 1)],
        );
    }
    let current = ThreadSpawnEdgeAuthority {
        thread_id: child,
        parent_thread_id: destination,
        owner_session_id: Some(destination.into()),
    };
    for expected in [true, false] {
        assert_eq!(
            runtime
                .close_thread_spawn_edge_if_current(current, &[child])
                .await
                .unwrap(),
            expected,
        );
        assert_eq!(
            runtime
                .read_agent_thread_lifecycle_epochs(&[child])
                .await
                .unwrap(),
            vec![(child, 2)],
        );
    }
}

#[tokio::test]
async fn epoch_migration_preserves_integrated_later_numbered_deletion_tombstones() {
    let home = tempfile::tempdir().unwrap();
    let config = SqliteConfig::new_for_testing(home.path().abs());
    let old_migrator = Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version != 10_053)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
    };
    let pool = config
        .open_read_write_pool(&config.state_db_path())
        .await
        .unwrap();
    old_migrator.run(&pool).await.unwrap();
    let deleted = ThreadId::new();
    sqlx::query("INSERT INTO agent_alias_tombstones (thread_id) VALUES (?)")
        .bind(deleted.to_string())
        .execute(&pool)
        .await
        .unwrap();
    STATE_MIGRATOR.run(&pool).await.unwrap();
    pool.close().await;
    let runtime = StateRuntime::init(config, "test-provider".to_string())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .read_agent_thread_lifecycle_epochs(&[deleted])
            .await
            .unwrap(),
        vec![(deleted, 0)],
    );
    let root = ThreadId::new();
    assert!(
        runtime
            .allocate_agent_alias(AgentAliasAllocation {
                session_id: root.into(),
                parent_thread_id: root,
                child_thread_id: deleted,
                nickname: None,
                task_path: None,
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("permanently deleted")
    );
}

#[tokio::test]
async fn exhausted_epoch_rolls_back_close_and_other_subtree_epoch_updates() {
    let home = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap();
    let root = ThreadId::new();
    let target = ThreadId::from_string("00000000-0000-0000-0000-000000000001").unwrap();
    let child = ThreadId::from_string("00000000-0000-0000-0000-000000000002").unwrap();
    for (parent, thread_id) in [(root, target), (target, child)] {
        runtime
            .allocate_agent_alias(AgentAliasAllocation {
                session_id: root.into(),
                parent_thread_id: parent,
                child_thread_id: thread_id,
                nickname: None,
                task_path: None,
            })
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO agent_thread_lifecycle_authority_epochs (thread_id, epoch) VALUES (?, ?)",
    )
    .bind(child.to_string())
    .bind(i64::MAX)
    .execute(runtime.pool.as_ref())
    .await
    .unwrap();
    let aliases = runtime.list_agent_aliases(root.into()).await.unwrap();
    for owned in [true, false] {
        let failed = if owned {
            runtime
                .set_agent_lifecycle_state_with_authority_revocations(
                    root.into(),
                    target,
                    DirectionalThreadSpawnEdgeStatus::Closed,
                    &[target, child],
                )
                .await
                .is_err()
        } else {
            runtime
                .set_thread_spawn_edge_status_with_authority_revocations(
                    target,
                    DirectionalThreadSpawnEdgeStatus::Closed,
                    &[target, child],
                )
                .await
                .is_err()
        };
        assert!(failed);
        assert_eq!(
            runtime.list_agent_aliases(root.into()).await.unwrap(),
            aliases
        );
        assert_eq!(
            runtime
                .read_agent_thread_lifecycle_epochs(&[target, child])
                .await
                .unwrap(),
            vec![(target, 0), (child, i64::MAX)],
        );
    }
}

#[tokio::test]
async fn fallback_missing_and_repeated_close_do_not_revoke_authority() {
    let home = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap();
    let root = ThreadId::new();
    let target = ThreadId::new();
    assert!(
        !runtime
            .set_thread_spawn_edge_status_with_authority_revocations(
                target,
                DirectionalThreadSpawnEdgeStatus::Closed,
                &[target],
            )
            .await
            .unwrap()
    );
    runtime
        .upsert_thread_spawn_edge(root, target, DirectionalThreadSpawnEdgeStatus::Open)
        .await
        .unwrap();
    for expected in [true, false] {
        assert_eq!(
            runtime
                .set_thread_spawn_edge_status_with_authority_revocations(
                    target,
                    DirectionalThreadSpawnEdgeStatus::Closed,
                    &[target, target],
                )
                .await
                .unwrap(),
            expected,
        );
        assert_eq!(
            runtime
                .read_agent_thread_lifecycle_epochs(&[target])
                .await
                .unwrap(),
            vec![(target, 1)],
        );
    }
}
