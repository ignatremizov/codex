use std::io::SeekFrom;
use std::path::Path;

use chrono::DateTime;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutLine;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tracing::warn;

use super::LocalThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const PROJECTION_READ_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROJECTION_RECORD_BYTES: usize = 256 * 1024 * 1024;

pub(super) async fn materialize_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    let (mut start_offset, next_ordinal) =
        super::thread_history::projection_state(store, thread_id).await?;
    let rollout_len = match tokio::fs::metadata(rollout_path).await {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => 0,
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let offset_is_valid = if start_offset == 0 {
        next_ordinal == 0
    } else if start_offset < rollout_len {
        // Incremental replay below validates that this is a record boundary and that the first
        // suffix ordinal matches `next_ordinal`. Avoid reverse-parsing the previous record on
        // every ordinary append.
        next_ordinal > 0
    } else if start_offset == rollout_len
        && let Some(expected_previous_ordinal) = next_ordinal.checked_sub(1)
    {
        // A vacuumed rollout can shrink to exactly a lagging projection offset, leaving no suffix
        // for incremental replay to validate. Check the preceding canonical ordinal in that case.
        let rollout_path = rollout_path.to_path_buf();
        let previous_ordinal = tokio::task::spawn_blocking(move || {
            codex_rollout::last_rollout_ordinal_before_offset(rollout_path.as_path(), start_offset)
        })
        .await
        .map_err(thread_history_error)?
        .map_err(thread_store_io_error)?;
        previous_ordinal == Some(expected_previous_ordinal)
    } else {
        false
    };
    if !offset_is_valid {
        warn!(
            "resetting paginated history projection after canonical rollout changed for {thread_id}"
        );
        return rebuild_to_sqlite(store, thread_id, rollout_path).await;
    }
    let subagent_history_start_ordinal = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta
        .subagent_history_start_ordinal;

    loop {
        let projection_result: ThreadStoreResult<Option<u64>> = async {
            let (lines, next_offset, rejected_line_count) =
                read_complete_rollout_lines(rollout_path, start_offset).await?;
            if rejected_line_count > 0 && start_offset > 0 {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "incremental rollout projection for {thread_id} started inside a rejected record"
                    ),
                });
            }
            if lines.is_empty() && start_offset == next_offset {
                return Ok(None);
            }
            let projections = project_lines(lines.as_slice(), subagent_history_start_ordinal)?;
            super::thread_history::apply_projection(
                store,
                thread_id,
                start_offset,
                next_offset,
                projections,
            )
            .await?;
            Ok(Some(next_offset))
        }
        .await;
        match projection_result {
            Ok(Some(next_offset)) => start_offset = next_offset,
            Ok(None) => return Ok(()),
            Err(err) if start_offset > 0 => {
                warn!(
                    "rebuilding paginated history projection after incremental replay failed for {thread_id}: {err}"
                );
                return rebuild_to_sqlite(store, thread_id, rollout_path).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub(super) async fn rebuild_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    let subagent_history_start_ordinal = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta
        .subagent_history_start_ordinal;
    let pool = store.thread_history_db().await?;
    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(thread_history_error)?;
    let thread_id_text = thread_id.to_string();
    sqlx::query("DELETE FROM thread_items WHERE thread_id = ?")
        .bind(thread_id_text.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(thread_history_error)?;
    sqlx::query("DELETE FROM thread_turns WHERE thread_id = ?")
        .bind(thread_id_text.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(thread_history_error)?;
    sqlx::query("DELETE FROM thread_history_projection_state WHERE thread_id = ?")
        .bind(thread_id_text.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(thread_history_error)?;

    let mut start_offset = 0u64;
    let mut next_ordinal = 0i64;
    loop {
        let (lines, next_offset, _rejected_line_count) =
            read_complete_rollout_lines(rollout_path, start_offset).await?;
        let projections = project_lines(lines.as_slice(), subagent_history_start_ordinal)?;
        for (ordinal, created_at_ms, changes) in projections {
            let ordinal = ordinal
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: format!(
                        "paginated rollout line for {thread_id_text} is missing an ordinal"
                    ),
                })
                .and_then(|ordinal| {
                    i64::try_from(ordinal).map_err(|_| ThreadStoreError::Internal {
                        message: "rollout ordinal exceeds SQLite integer range".to_string(),
                    })
                })?;
            if ordinal != next_ordinal {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "thread history projection for {thread_id_text} expected ordinal {next_ordinal}, got {ordinal}"
                    ),
                });
            }
            super::thread_history::apply_change_set(
                &mut transaction,
                thread_id_text.as_str(),
                ordinal,
                created_at_ms,
                changes,
            )
            .await?;
            next_ordinal =
                next_ordinal
                    .checked_add(1)
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: "rollout ordinal exceeds SQLite integer range".to_string(),
                    })?;
        }
        if next_offset == start_offset {
            break;
        }
        start_offset = next_offset;
    }

    sqlx::query(
        r#"
INSERT INTO thread_history_projection_state (
    thread_id,
    next_rollout_byte_offset,
    next_rollout_ordinal
) VALUES (?, ?, ?)
        "#,
    )
    .bind(thread_id_text.as_str())
    .bind(
        i64::try_from(start_offset).map_err(|_| ThreadStoreError::Internal {
            message: "rollout byte offset exceeds SQLite integer range".to_string(),
        })?,
    )
    .bind(next_ordinal)
    .execute(&mut *transaction)
    .await
    .map_err(thread_history_error)?;
    transaction.commit().await.map_err(thread_history_error)
}

fn project_lines(
    lines: &[RolloutLine],
    subagent_history_start_ordinal: Option<u64>,
) -> ThreadStoreResult<Vec<(Option<u64>, i64, ThreadHistoryChangeSet)>> {
    lines
        .iter()
        .map(|line| {
            let created_at_ms = DateTime::parse_from_rfc3339(line.timestamp.as_str())
                .map(|timestamp| timestamp.timestamp_millis())
                .map_err(thread_history_error)?;
            let changes = if line.ordinal.is_some_and(|ordinal| {
                subagent_history_start_ordinal.is_some_and(|start| ordinal < start)
            }) {
                ThreadHistoryChangeSet::default()
            } else {
                project_rollout_line(line)
            };
            Ok((line.ordinal, created_at_ms, changes))
        })
        .collect()
}

async fn read_complete_rollout_lines(
    rollout_path: &Path,
    start_offset: u64,
) -> ThreadStoreResult<(Vec<RolloutLine>, u64, usize)> {
    let next_offset = match tokio::fs::metadata(rollout_path).await {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => {
            return Ok((Vec::new(), 0, 0));
        }
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let remaining_byte_count =
        next_offset
            .checked_sub(start_offset)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout shrank before projection".to_string(),
            })?;
    let remaining_byte_count =
        usize::try_from(remaining_byte_count).map_err(|_| ThreadStoreError::Internal {
            message: "durable rollout append exceeds addressable memory".to_string(),
        })?;
    if remaining_byte_count == 0 {
        return Ok((Vec::new(), start_offset, 0));
    }
    let mut byte_count = remaining_byte_count.min(PROJECTION_READ_BATCH_BYTES);
    let mut file = tokio::fs::File::open(rollout_path)
        .await
        .map_err(thread_store_io_error)?;
    let (bytes, complete_byte_count) = loop {
        let mut bytes = vec![0; byte_count];
        file.seek(SeekFrom::Start(start_offset))
            .await
            .map_err(thread_store_io_error)?;
        file.read_exact(bytes.as_mut_slice())
            .await
            .map_err(thread_store_io_error)?;
        let complete_byte_count = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if complete_byte_count > 0 || byte_count == remaining_byte_count {
            break (bytes, complete_byte_count);
        }
        if byte_count >= MAX_PROJECTION_RECORD_BYTES {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "rollout record exceeds the {MAX_PROJECTION_RECORD_BYTES} byte history projection limit"
                ),
            });
        }
        byte_count = byte_count
            .saturating_mul(2)
            .min(MAX_PROJECTION_RECORD_BYTES)
            .min(remaining_byte_count);
    };
    let next_offset = start_offset
        .checked_add(u64::try_from(complete_byte_count).map_err(|_| {
            ThreadStoreError::Internal {
                message: "durable rollout append exceeds addressable memory".to_string(),
            }
        })?)
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "durable rollout byte offset overflow".to_string(),
        })?;
    let text = std::str::from_utf8(&bytes[..complete_byte_count]).map_err(thread_history_error)?;
    let mut lines = Vec::new();
    let mut rejected_line_count = 0usize;
    for line in text.lines().filter(|line| !line.is_empty()) {
        match serde_json::from_str(line) {
            Ok(line) => lines.push(line),
            Err(err) => {
                // A failed append can leave a partial record behind. The rollout writer repairs
                // its newline before retrying, so skip rejected lines just like the canonical
                // rollout loader and keep projecting the valid retry that follows.
                warn!("skipping rejected rollout line while projecting {rollout_path:?}: {err}");
                rejected_line_count = rejected_line_count.saturating_add(1);
            }
        }
    }
    Ok((lines, next_offset, rejected_line_count))
}

fn thread_history_error(err: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("failed to project thread history: {err}"),
    }
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[path = "thread_history_materialization_tests.rs"]
mod tests;
