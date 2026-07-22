use std::io::SeekFrom;
use std::path::Path;

use chrono::DateTime;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::rollout::exact_rollback_removed_items;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tracing::warn;

use super::LocalThreadStore;
use super::thread_history::ProjectedRolloutLine;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

struct CompleteRolloutLine {
    line: RolloutLine,
    start_byte_offset: u64,
    end_byte_offset: u64,
}

pub(super) async fn materialize_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    if store.state_db.is_none() {
        return Ok(());
    }
    let projection_state = super::thread_history::projection_state(store, thread_id).await?;
    let (start_offset, next_ordinal) = projection_state
        .map(|state| (state.next_byte_offset, state.next_ordinal))
        .unwrap_or((0, 0));
    let rollout_len = tokio::fs::metadata(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .len();
    let offset_is_valid = if start_offset == 0 {
        next_ordinal == 0
    } else if start_offset <= rollout_len
        && let Some(expected_previous_ordinal) = next_ordinal.checked_sub(1)
    {
        let rollout_path = rollout_path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            codex_rollout::last_rollout_ordinal_before_offset(rollout_path.as_path(), start_offset)
        })
        .await
        .map_err(thread_history_error)?
        .map_err(thread_store_io_error)?
            == Some(expected_previous_ordinal)
    } else {
        false
    };
    if !offset_is_valid {
        warn!(
            "rebuilding paginated history projection after canonical rollout changed for {thread_id}"
        );
        return rebuild_to_sqlite(store, thread_id, rollout_path).await;
    }
    let (lines, next_offset) = read_complete_rollout_lines(rollout_path, start_offset).await?;
    // Empty valid records can still consume bytes through blank or rejected complete lines.
    if lines.is_empty() && start_offset == next_offset {
        return Ok(());
    }
    let session_meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let initial_ordinal = session_meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
    if lines.iter().any(|record| {
        matches!(
            &record.line.item,
            codex_protocol::protocol::RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback))
                if rollback.rollback_start_index.is_some()
        )
    }) {
        return rebuild_to_sqlite(store, thread_id, rollout_path).await;
    }

    let projections = lines
        .iter()
        .map(|record| {
            let line = &record.line;
            let ordinal = line.ordinal.ok_or_else(|| ThreadStoreError::Internal {
                message: format!("paginated rollout line for {thread_id} is missing an ordinal"),
            })?;
            let created_at_ms = DateTime::parse_from_rfc3339(line.timestamp.as_str())
                .map(|timestamp| timestamp.timestamp_millis())
                .map_err(thread_history_error)?;
            let changes = if subagent_history_start_ordinal.is_some_and(|start| ordinal < start) {
                ThreadHistoryChangeSet::default()
            } else {
                project_rollout_line(line)
            };
            Ok(ProjectedRolloutLine {
                ordinal,
                start_byte_offset: record.start_byte_offset,
                end_byte_offset: record.end_byte_offset,
                created_at_ms,
                changes,
            })
        })
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    super::thread_history::apply_projection(
        store,
        thread_id,
        start_offset,
        next_offset,
        initial_ordinal,
        projections,
    )
    .await
}

pub(super) async fn rebuild_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    let (lines, next_offset) =
        read_complete_rollout_lines(rollout_path, /*start_offset*/ 0).await?;
    let session_meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let initial_ordinal = session_meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
    let rollout_items = lines
        .iter()
        .map(|record| record.line.item.clone())
        .collect::<Vec<_>>();
    let removed_items = exact_rollback_removed_items(&rollout_items);

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

    let mut next_ordinal = initial_ordinal;
    let mut history_builder = ThreadHistoryBuilder::new();
    for (record, removed) in lines.iter().zip(removed_items) {
        let line = &record.line;
        let ordinal = line.ordinal.ok_or_else(|| ThreadStoreError::Internal {
            message: format!("paginated rollout line for {thread_id_text} is missing an ordinal"),
        })?;
        if ordinal != next_ordinal {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id_text} expected ordinal {next_ordinal}, got {ordinal}"
                ),
            });
        }
        let created_at_ms = DateTime::parse_from_rfc3339(line.timestamp.as_str())
            .map(|timestamp| timestamp.timestamp_millis())
            .map_err(thread_history_error)?;
        let skipped =
            removed || subagent_history_start_ordinal.is_some_and(|start| ordinal < start);
        let mut changes = if skipped {
            history_builder.skip_rollout_item();
            ThreadHistoryChangeSet::default()
        } else {
            history_builder.handle_rollout_item_with_changes(&line.item)
        };
        if !skipped {
            let stateless_changes = project_rollout_line(line);
            for item in stateless_changes.changed_items {
                if !changes.changed_items.iter().any(|existing| {
                    existing.turn_id == item.turn_id && existing.item.id() == item.item.id()
                }) {
                    changes.changed_items.push(item);
                }
            }
            for turn in stateless_changes.changed_turns {
                if !changes
                    .changed_turns
                    .iter()
                    .any(|existing| existing.turn_id == turn.turn_id)
                {
                    changes.changed_turns.push(turn);
                }
            }
        }
        super::thread_history::apply_change_set(
            &mut transaction,
            thread_id_text.as_str(),
            sqlite_integer(ordinal, "rollout ordinal")?,
            sqlite_integer(record.start_byte_offset, "rollout byte offset")?,
            sqlite_integer(record.end_byte_offset, "rollout byte offset")?,
            created_at_ms,
            changes,
        )
        .await?;
        next_ordinal = next_ordinal
            .checked_add(1)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "rollout ordinal overflow".to_string(),
            })?;
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
    .bind(thread_id_text)
    .bind(sqlite_integer(next_offset, "rollout byte offset")?)
    .bind(sqlite_integer(next_ordinal, "rollout ordinal")?)
    .execute(&mut *transaction)
    .await
    .map_err(thread_history_error)?;
    transaction.commit().await.map_err(thread_history_error)
}

async fn read_complete_rollout_lines(
    rollout_path: &Path,
    start_offset: u64,
) -> ThreadStoreResult<(Vec<CompleteRolloutLine>, u64)> {
    let next_offset = match tokio::fs::metadata(rollout_path).await {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => {
            return Ok((Vec::new(), 0));
        }
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let byte_count =
        next_offset
            .checked_sub(start_offset)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout shrank before projection".to_string(),
            })?;
    let byte_count = usize::try_from(byte_count).map_err(|_| ThreadStoreError::Internal {
        message: "durable rollout append exceeds addressable memory".to_string(),
    })?;
    let mut bytes = vec![0; byte_count];
    let mut file = tokio::fs::File::open(rollout_path)
        .await
        .map_err(thread_store_io_error)?;
    file.seek(SeekFrom::Start(start_offset))
        .await
        .map_err(thread_store_io_error)?;
    file.read_exact(bytes.as_mut_slice())
        .await
        .map_err(thread_store_io_error)?;
    // Only project the newline-terminated prefix; leave a trailing partial record for the next
    // pass.
    let complete_byte_count = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let next_offset = start_offset
        .checked_add(u64::try_from(complete_byte_count).map_err(|_| {
            ThreadStoreError::Internal {
                message: "durable rollout append exceeds addressable memory".to_string(),
            }
        })?)
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "durable rollout byte offset overflow".to_string(),
        })?;
    let mut lines = Vec::new();
    let mut line_start_offset = start_offset;
    // Preserve each complete physical line's trailing newline so byte offsets advance through
    // every durable byte, including blank or rejected lines that do not project a row.
    for line_bytes in bytes[..complete_byte_count].split_inclusive(|byte| *byte == b'\n') {
        let line_end_offset = line_start_offset
            .checked_add(u64::try_from(line_bytes.len()).map_err(|_| {
                ThreadStoreError::Internal {
                    message: "durable rollout byte offset overflow".to_string(),
                }
            })?)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout byte offset overflow".to_string(),
            })?;
        // Blank physical lines consume bytes but are not rollout records.
        if !line_bytes.iter().all(u8::is_ascii_whitespace) {
            match serde_json::from_slice::<serde_json::Value>(line_bytes)
                .and_then(serde_json::from_value::<RolloutLine>)
            {
                Ok(line) => lines.push(CompleteRolloutLine {
                    line,
                    start_byte_offset: line_start_offset,
                    end_byte_offset: line_end_offset,
                }),
                Err(err) => {
                    // A failed append can leave a partial record behind. The rollout writer
                    // repairs its newline before retrying, so skip rejected lines just like the
                    // canonical rollout loader and keep projecting the valid retry that follows.
                    warn!(
                        "skipping rejected rollout line while projecting {rollout_path:?}: {err}"
                    );
                }
            }
        }
        line_start_offset = line_end_offset;
    }
    Ok((lines, next_offset))
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

fn sqlite_integer(value: u64, field: &str) -> ThreadStoreResult<i64> {
    i64::try_from(value).map_err(|_| ThreadStoreError::Internal {
        message: format!("{field} exceeds SQLite integer range"),
    })
}

#[cfg(test)]
#[path = "thread_history_materialization_tests.rs"]
mod tests;
