//! Local hard-delete support for persisted threads.
//!
//! Existing rollout files are deleted before this operation reports success. A rollout file that
//! vanishes after discovery counts as already deleted. The app-server deletes main state DB rows
//! after every associated rollout is removed; this module deletes local history projection rows.

use std::io::ErrorKind;
use std::path::Path;

use codex_rollout::ARCHIVED_SESSIONS_SUBDIR;
use codex_rollout::SESSIONS_SUBDIR;
use codex_rollout::compacted_media_vacuum_artifact_rollout_file_name;
use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::find_thread_path_by_id_str;
use codex_rollout::remove_compacted_media_vacuum_backups;
use codex_rollout::remove_thread_name_entries;
use codex_rollout::rollout_date_parts;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::scoped_rollout_path;
use crate::DeleteThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let _live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    let thread_id_str = thread_id.to_string();
    let state_db_ctx = store.state_db().await;
    let mut rollout_paths = Vec::new();
    match find_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) => rollout_paths.push(path),
        Ok(None) => {}
        Err(err) => {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("failed to locate thread id {thread_id}: {err}"),
            });
        }
    }
    match find_archived_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) => {
            if !rollout_paths.contains(&path) {
                rollout_paths.push(path);
            }
        }
        Ok(None) => {}
        Err(err) => {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("failed to locate archived thread id {thread_id}: {err}"),
            });
        }
    }
    // Stop the live writer before removing files. The per-thread lock keeps new writes and
    // replacements out while we find paths and clean up rollout files and history rows.
    store.live_recorders.lock().await.remove(&thread_id);
    let removed_orphaned_artifact = if rollout_paths.is_empty() {
        delete_vacuum_artifact_only_rollouts(store, thread_id)?
    } else {
        false
    };
    let found_rollout_path = !rollout_paths.is_empty() || removed_orphaned_artifact;
    for rollout_path in rollout_paths {
        delete_rollout_file(store, rollout_path.as_path(), thread_id)?;
    }
    remove_thread_name_entries(store.config.codex_home.as_path(), thread_id)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to delete thread name index entries for {thread_id}: {err}"),
        })?;
    // Keep this before ThreadNotFound so a retry can finish cleanup after an earlier attempt
    // already removed the rollout file.
    super::thread_history::delete_thread(store, thread_id).await?;

    if !found_rollout_path {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }

    Ok(())
}

fn delete_vacuum_artifact_only_rollouts(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let mut stack = vec![
        store.config.codex_home.join(SESSIONS_SUBDIR),
        store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
    ];
    let mut removed = false;
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(directory.as_path()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "failed to scan for compacted-media vacuum backups in `{}`: {err}",
                        directory.display()
                    ),
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|err| ThreadStoreError::Internal {
                message: format!(
                    "failed to scan for compacted-media vacuum backups in `{}`: {err}",
                    directory.display()
                ),
            })?;
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(ThreadStoreError::Internal {
                        message: format!(
                            "failed to inspect compacted-media vacuum backup `{}`: {err}",
                            entry.path().display()
                        ),
                    });
                }
            };
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(canonical_file_name) = file_name
                .to_str()
                .and_then(compacted_media_vacuum_artifact_rollout_file_name)
            else {
                continue;
            };
            let canonical_path = entry.path().with_file_name(canonical_file_name);
            if canonical_path
                .file_name()
                .and_then(rollout_date_parts)
                .is_none()
            {
                continue;
            }
            if matching_rollout_file_name(&canonical_path, thread_id, entry.path().as_path())
                .is_err()
            {
                continue;
            }
            remove_compacted_media_vacuum_backups(canonical_path.as_path()).map_err(|err| {
                ThreadStoreError::Internal {
                    message: format!(
                        "failed to delete compacted-media vacuum backups for `{}`: {err}",
                        canonical_path.display()
                    ),
                }
            })?;
            removed = true;
        }
    }
    Ok(removed)
}

fn delete_rollout_file(
    store: &LocalThreadStore,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let plain_path = codex_rollout::plain_rollout_path(rollout_path);
    let compressed_path = plain_path.with_extension("jsonl.zst");
    let deleted_plain = delete_rollout_path(store, plain_path.as_path(), thread_id)?;
    let deleted_compressed = delete_rollout_path(store, compressed_path.as_path(), thread_id)?;
    Ok(deleted_plain || deleted_compressed)
}

fn delete_rollout_path(
    store: &LocalThreadStore,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let canonical_rollout_path = scoped_rollout_path(
        store.config.codex_home.join(SESSIONS_SUBDIR),
        rollout_path,
        "sessions",
    )
    .or_else(|_| {
        scoped_rollout_path(
            store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
            rollout_path,
            "archived sessions",
        )
    })
    .or_else(|err| match rollout_path.try_exists() {
        Ok(false) => Ok(rollout_path.to_path_buf()),
        Ok(true) | Err(_) => Err(err),
    })?;
    matching_rollout_file_name(&canonical_rollout_path, thread_id, rollout_path)?;
    remove_compacted_media_vacuum_backups(&canonical_rollout_path).map_err(|err| {
        ThreadStoreError::Internal {
            message: format!(
                "failed to delete compacted-media vacuum backups for `{}`: {err}",
                canonical_rollout_path.display()
            ),
        }
    })?;
    match std::fs::remove_file(&canonical_rollout_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!(
                "failed to delete rollout file `{}`: {err}",
                canonical_rollout_path.display()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use codex_protocol::ThreadId;
    use codex_protocol::protocol::ThreadHistoryMode;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::ThreadStore;
    use crate::local::LocalThreadStore;
    use crate::local::test_support::test_config;
    use crate::local::test_support::write_archived_session_file;
    use crate::local::test_support::write_session_file;
    use crate::local::test_support::write_session_file_with_history_mode;

    #[tokio::test]
    async fn delete_thread_removes_active_and_archived_rollouts() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let active_path =
            write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(301))
                .expect("session file");
        let compressed_path = active_path.with_extension("jsonl.zst");
        std::fs::write(&compressed_path, b"compressed sibling").expect("compressed sibling");
        let cases = [
            (Uuid::from_u128(301), active_path),
            (
                Uuid::from_u128(302),
                write_archived_session_file(
                    home.path(),
                    "2025-01-03T12-00-00",
                    Uuid::from_u128(302),
                )
                .expect("archived session file"),
            ),
        ];

        for (uuid, path) in cases {
            let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
            let backup_path = path.with_file_name(format!(
                ".{}.pre-media-vacuum-{}.bak",
                path.file_name()
                    .and_then(|file_name| file_name.to_str())
                    .expect("UTF-8 rollout file name"),
                Uuid::now_v7()
            ));
            std::fs::hard_link(&path, &backup_path).expect("retained vacuum backup");
            store
                .delete_thread(DeleteThreadParams { thread_id })
                .await
                .expect("delete thread");

            assert!(!path.exists());
            assert!(!backup_path.exists());
        }
        assert!(!compressed_path.exists());
    }

    #[tokio::test]
    async fn delete_rollout_file_treats_vanished_path_as_already_deleted() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(305);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        std::fs::remove_file(&path).expect("remove session file");

        assert!(!delete_rollout_file(&store, path.as_path(), thread_id).expect("delete rollout"));
    }

    #[tokio::test]
    async fn delete_thread_removes_a_marker_invalid_backup_only_rollout() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(307);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open rollout"),
            "{}",
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": 7,
                "payload": {
                    "replacement_history_media_sanitized_prefix_len": 0
                }
            })
        )
        .expect("append protected checkpoint");
        let backup_path = path.with_file_name(format!(
            ".{}.pre-media-vacuum-{}.bak",
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .expect("UTF-8 rollout file name"),
            Uuid::now_v7()
        ));
        std::fs::hard_link(&path, &backup_path).expect("vacuum backup");
        std::fs::remove_file(&path).expect("remove canonical rollout");

        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("delete backup-only thread");

        assert!(!path.exists());
        assert!(!backup_path.exists());
    }

    #[tokio::test]
    async fn delete_thread_removes_a_temporary_artifact_only_rollout() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(308);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        let temporary_path = path.with_file_name(format!(
            ".{}.media-vacuum-interrupted.tmp",
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .expect("UTF-8 rollout file name")
        ));
        std::fs::write(&temporary_path, b"interrupted vacuum output")
            .expect("vacuum temporary file");
        std::fs::remove_file(&path).expect("remove canonical rollout");

        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("delete temporary-only thread");

        assert!(!path.exists());
        assert!(!temporary_path.exists());
    }

    #[tokio::test]
    async fn delete_thread_removes_materialized_thread_history() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(306);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        write_session_file_with_history_mode(
            home.path(),
            "2025-01-03T12-00-00",
            uuid,
            ThreadHistoryMode::Paginated,
        )
        .expect("session file");
        let pool = store
            .thread_history_db()
            .await
            .expect("open thread history db")
            .clone();
        let thread_id_string = thread_id.to_string();
        sqlx::query(
            "INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status) VALUES (?, 'turn-1', 1, 'completed')",
        )
        .bind(thread_id_string.as_str())
        .execute(&pool)
        .await
        .expect("insert turn");
        sqlx::query(
            "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES (?, 'turn-1', 'item-1', 2, 1, '{}')",
        )
        .bind(thread_id_string.as_str())
        .execute(&pool)
        .await
        .expect("insert item");
        sqlx::query(
            "INSERT INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES (?, 3, 3)",
        )
        .bind(thread_id_string.as_str())
        .execute(&pool)
        .await
        .expect("insert projection state");
        sqlx::query(
            "INSERT INTO fork_thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES (?, 3, 3)",
        )
        .bind(thread_id_string.as_str())
        .execute(&pool)
        .await
        .expect("insert fork projection state");

        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("delete thread");

        let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            r#"
SELECT
    (SELECT COUNT(*) FROM thread_turns WHERE thread_id = ?),
    (SELECT COUNT(*) FROM thread_items WHERE thread_id = ?),
    (SELECT COUNT(*) FROM thread_history_projection_state WHERE thread_id = ?),
    (SELECT COUNT(*) FROM fork_thread_history_projection_state WHERE thread_id = ?)
            "#,
        )
        .bind(thread_id_string.as_str())
        .bind(thread_id_string.as_str())
        .bind(thread_id_string.as_str())
        .bind(thread_id_string.as_str())
        .fetch_one(&pool)
        .await
        .expect("read remaining history rows");
        assert_eq!(counts, (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn delete_thread_reports_missing_thread() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000304").expect("valid thread id");

        let err = store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect_err("missing thread should fail");
        assert_eq!(
            err.to_string(),
            "thread 00000000-0000-0000-0000-000000000304 not found"
        );
    }
}
