use std::io::SeekFrom;
use std::path::Path;

use chrono::DateTime;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::rollout::exact_rollback_removed_items;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tracing::warn;

use super::LocalThreadStore;
use super::thread_history::ProjectedRolloutLine;
use super::thread_history::RolloutProjectionStep;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

enum ParsedProjectionStep {
    Line {
        line: Box<RolloutLine>,
        projection: ProjectedRolloutLine,
    },
    SkippedOrdinalRange {
        start_ordinal: u64,
        end_ordinal_exclusive: u64,
    },
}

impl ParsedProjectionStep {
    fn into_projection_step(self) -> RolloutProjectionStep {
        match self {
            Self::Line { projection, .. } => RolloutProjectionStep::Line(projection),
            Self::SkippedOrdinalRange {
                start_ordinal,
                end_ordinal_exclusive,
            } => RolloutProjectionStep::SkippedOrdinalRange {
                start_ordinal,
                end_ordinal_exclusive,
            },
        }
    }
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
    let start_offset = projection_state
        .as_ref()
        .map_or(0, |state| state.next_byte_offset);
    if projection_state.is_none()
        && codex_rollout::existing_rollout_path(rollout_path)
            .await
            .is_none()
    {
        return Ok(());
    }
    let offset_is_valid = if let Some(projection_state) = projection_state.as_ref() {
        let rollout_len = tokio::fs::metadata(rollout_path)
            .await
            .map_err(thread_store_io_error)?
            .len();
        if start_offset == 0 {
            projection_state.next_ordinal == 0
        } else if start_offset <= rollout_len
            && let Some(expected_previous_ordinal) = projection_state.next_ordinal.checked_sub(1)
        {
            let rollout_path = rollout_path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                codex_rollout::last_rollout_ordinal_before_offset(
                    rollout_path.as_path(),
                    start_offset,
                )
            })
            .await
            .map_err(thread_history_error)?
            .map_err(thread_store_io_error)?
                == Some(expected_previous_ordinal)
        } else {
            false
        }
    } else {
        true
    };
    if !offset_is_valid {
        warn!(
            "rebuilding paginated history projection after canonical rollout changed for {thread_id}"
        );
        return rebuild_to_sqlite(store, thread_id, rollout_path).await;
    }
    let session_meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let initial_ordinal = session_meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
    let expected_ordinal = projection_state
        .as_ref()
        .map_or(initial_ordinal, |state| state.next_ordinal);
    let (parsed_steps, next_offset) = read_projection_steps(
        rollout_path,
        start_offset,
        expected_ordinal,
        thread_id,
        subagent_history_start_ordinal,
    )
    .await?;
    // Empty valid records can still consume bytes through blank complete lines.
    if parsed_steps.is_empty() && start_offset == next_offset {
        return Ok(());
    }
    if parsed_steps.iter().any(|step| match step {
        ParsedProjectionStep::Line { line, .. } => matches!(
            &line.item,
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback))
                if rollback.rollback_start_index.is_some()
        ),
        ParsedProjectionStep::SkippedOrdinalRange { .. } => false,
    }) {
        return rebuild_to_sqlite(store, thread_id, rollout_path).await;
    }
    let projections = parsed_steps
        .into_iter()
        .map(ParsedProjectionStep::into_projection_step)
        .collect();
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
    let session_meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let initial_ordinal = session_meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
    let (parsed_steps, next_offset) = read_projection_steps(
        rollout_path,
        /*start_offset*/ 0,
        initial_ordinal,
        thread_id,
        subagent_history_start_ordinal,
    )
    .await?;
    let mut rollout_items = Vec::new();
    for step in &parsed_steps {
        match step {
            ParsedProjectionStep::Line { line, .. } => rollout_items.push(line.item.clone()),
            ParsedProjectionStep::SkippedOrdinalRange {
                start_ordinal,
                end_ordinal_exclusive,
            } => {
                for _ in *start_ordinal..*end_ordinal_exclusive {
                    // Preserve raw rollout indexes for forward-unknown records. Exact rollback
                    // boundaries count every durable ordinal even when this binary cannot project
                    // the record itself.
                    rollout_items.push(RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: false,
                    });
                }
            }
        }
    }
    let mut removed_items = exact_rollback_removed_items(&rollout_items).into_iter();

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
    for step in parsed_steps {
        match step {
            ParsedProjectionStep::SkippedOrdinalRange {
                start_ordinal,
                end_ordinal_exclusive,
            } => {
                if start_ordinal != next_ordinal {
                    return Err(ThreadStoreError::Internal {
                        message: format!(
                            "thread history projection for {thread_id_text} expected ordinal {next_ordinal}, got {start_ordinal}"
                        ),
                    });
                }
                for _ in start_ordinal..end_ordinal_exclusive {
                    removed_items
                        .next()
                        .ok_or_else(|| ThreadStoreError::Internal {
                            message: "exact rollback filter did not cover every rollout item"
                                .to_string(),
                        })?;
                    history_builder.skip_rollout_item();
                }
                next_ordinal = end_ordinal_exclusive;
            }
            ParsedProjectionStep::Line { line, projection } => {
                let ordinal = projection.ordinal;
                if ordinal != next_ordinal {
                    return Err(ThreadStoreError::Internal {
                        message: format!(
                            "thread history projection for {thread_id_text} expected ordinal {next_ordinal}, got {ordinal}"
                        ),
                    });
                }
                let removed = removed_items
                    .next()
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: "exact rollback filter did not cover every rollout item"
                            .to_string(),
                    })?;
                let skipped =
                    removed || subagent_history_start_ordinal.is_some_and(|start| ordinal < start);
                let mut changes = if skipped {
                    history_builder.skip_rollout_item();
                    ThreadHistoryChangeSet::default()
                } else {
                    history_builder.handle_rollout_item_with_changes(&line.item)
                };
                if !skipped {
                    let stateless_changes = projection.changes;
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
                let fallback_created_at_ms = if changes
                    .changed_items
                    .iter()
                    .any(|item| item.started_at_ms.is_none())
                {
                    Some(
                        DateTime::parse_from_rfc3339(line.timestamp.as_str())
                            .map(|timestamp| timestamp.timestamp_millis())
                            .map_err(thread_history_error)?,
                    )
                } else {
                    None
                };
                super::thread_history::apply_change_set(
                    &mut transaction,
                    thread_id_text.as_str(),
                    sqlite_integer(ordinal, "rollout ordinal")?,
                    sqlite_integer(projection.start_byte_offset, "rollout byte offset")?,
                    sqlite_integer(projection.end_byte_offset, "rollout byte offset")?,
                    fallback_created_at_ms,
                    changes,
                )
                .await?;
                next_ordinal =
                    next_ordinal
                        .checked_add(1)
                        .ok_or_else(|| ThreadStoreError::Internal {
                            message: "rollout ordinal overflow".to_string(),
                        })?;
            }
        }
    }
    if removed_items.next().is_some() {
        return Err(ThreadStoreError::Internal {
            message: "exact rollback filter produced extra rollout items".to_string(),
        });
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

async fn read_projection_steps(
    rollout_path: &Path,
    start_offset: u64,
    expected_ordinal: u64,
    thread_id: ThreadId,
    subagent_history_start_ordinal: Option<u64>,
) -> ThreadStoreResult<(Vec<ParsedProjectionStep>, u64)> {
    let path = rollout_path.to_path_buf();
    let file =
        tokio::task::spawn_blocking(move || codex_rollout::open_rollout_seekable_reader(&path))
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to join rollout projection read: {err}"),
            })?;
    let mut file = match file {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => {
            return Ok((Vec::new(), 0));
        }
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let file_end_offset = file.metadata().await.map_err(thread_store_io_error)?.len();
    let byte_count =
        file_end_offset
            .checked_sub(start_offset)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout shrank before projection".to_string(),
            })?;
    let byte_count = usize::try_from(byte_count).map_err(|_| ThreadStoreError::Internal {
        message: "durable rollout append exceeds addressable memory".to_string(),
    })?;
    let mut bytes = vec![0; byte_count];
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
    let mut projections = Vec::new();
    let mut next_ordinal = expected_ordinal;
    let mut next_offset = start_offset;
    let mut pending_rejected_line_count = 0;
    let mut line_start_offset = start_offset;
    // Keep rejected lines pending until a later valid ordinal proves whether they consumed history.
    // This lets a same-ordinal retry replace a failed write without advancing only one checkpoint.
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
        if line_bytes.iter().all(u8::is_ascii_whitespace) {
            if pending_rejected_line_count == 0 {
                next_offset = line_end_offset;
            }
            line_start_offset = line_end_offset;
            continue;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(line_bytes) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    error = %err,
                    "deferring rejected rollout line until a later ordinal resolves it"
                );
                pending_rejected_line_count += 1;
                line_start_offset = line_end_offset;
                continue;
            }
        };
        let value_ordinal = value.get("ordinal").and_then(serde_json::Value::as_u64);
        let line = match codex_rollout::decode_rollout_line(value) {
            Ok(line) => Some(line),
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    line_ordinal = ?value_ordinal,
                    error = %err,
                    "deferring unknown rollout line until a later ordinal resolves it"
                );
                None
            }
        };
        let ordinal = match line
            .as_ref()
            .and_then(|line| line.ordinal)
            .or(value_ordinal)
        {
            Some(ordinal) => ordinal,
            None if line.is_none() => {
                pending_rejected_line_count += 1;
                line_start_offset = line_end_offset;
                continue;
            }
            None => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "paginated rollout line for {thread_id} is missing an ordinal"
                    ),
                });
            }
        };
        if ordinal < next_ordinal {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}"
                ),
            });
        }
        let Some(line) = line else {
            pending_rejected_line_count += 1;
            line_start_offset = line_end_offset;
            continue;
        };
        let skipped_ordinal_count = ordinal - next_ordinal;
        if skipped_ordinal_count > pending_rejected_line_count {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}; {pending_rejected_line_count} rejected rollout lines cannot cover that gap"
                ),
            });
        }
        let is_inherited_subagent_history =
            subagent_history_start_ordinal.is_some_and(|start| ordinal < start);
        let changes = if is_inherited_subagent_history {
            ThreadHistoryChangeSet::default()
        } else {
            project_rollout_line(&line)
        };
        let fallback_created_at_ms = if changes
            .changed_items
            .iter()
            .any(|item| item.started_at_ms.is_none())
            || (!is_inherited_subagent_history
                && matches!(&line.item, RolloutItem::RealtimeItem(_)))
        {
            match DateTime::parse_from_rfc3339(line.timestamp.as_str()) {
                Ok(timestamp) => Some(timestamp.timestamp_millis()),
                Err(err) => {
                    warn!(
                        thread_id = %thread_id,
                        rollout_path = %rollout_path.display(),
                        line_start_byte_offset = line_start_offset,
                        line_end_byte_offset = line_end_offset,
                        expected_ordinal = next_ordinal,
                        line_ordinal = ordinal,
                        error = %err,
                        "deferring rollout line with invalid timestamp until a later ordinal resolves it"
                    );
                    pending_rejected_line_count += 1;
                    line_start_offset = line_end_offset;
                    continue;
                }
            }
        } else {
            None
        };
        if skipped_ordinal_count > 0 {
            warn!(
                thread_id = %thread_id,
                rollout_path = %rollout_path.display(),
                line_start_byte_offset = line_start_offset,
                line_end_byte_offset = line_end_offset,
                expected_ordinal = next_ordinal,
                line_ordinal = ordinal,
                skipped_ordinal_start = next_ordinal,
                skipped_ordinal_end_exclusive = ordinal,
                "skipping rollout ordinal range after rejected lines"
            );
            projections.push(ParsedProjectionStep::SkippedOrdinalRange {
                start_ordinal: next_ordinal,
                end_ordinal_exclusive: ordinal,
            });
        }
        pending_rejected_line_count = 0;
        let next_line_ordinal =
            ordinal
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "rollout ordinal exceeds SQLite integer range".to_string(),
                })?;
        let realtime_item = match &line.item {
            RolloutItem::RealtimeItem(item) if !is_inherited_subagent_history => Some(item.clone()),
            _ => None,
        };
        projections.push(ParsedProjectionStep::Line {
            line: Box::new(line),
            projection: ProjectedRolloutLine {
                ordinal,
                start_byte_offset: line_start_offset,
                end_byte_offset: line_end_offset,
                fallback_created_at_ms,
                changes,
                realtime_item,
            },
        });
        next_ordinal = next_line_ordinal;
        next_offset = line_end_offset;
        line_start_offset = line_end_offset;
    }
    Ok((projections, next_offset))
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
