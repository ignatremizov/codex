use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ARCHIVED_SESSIONS_SUBDIR;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::SESSIONS_SUBDIR;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;

use super::LocalThreadStore;
use super::read_thread;
use super::rollout_lineage::OpenedRolloutLineage;
use super::rollout_lineage::RolloutLineageSegment;
use crate::LoadForkSourceByRolloutPathParams;
use crate::StoredForkSource;
use crate::StoredThreadHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn load(
    store: &LocalThreadStore,
    params: LoadForkSourceByRolloutPathParams,
) -> ThreadStoreResult<StoredForkSource> {
    let rollout_path =
        read_thread::resolve_requested_rollout_path(store, params.rollout_path).await?;
    let mut source_file =
        codex_rollout::open_rollout_seekable_reader_without_recovery(rollout_path.as_path())
            .await
            .map_err(|err| lineage_read_error(rollout_path.as_path(), err))?;
    let source_byte_limit = complete_jsonl_prefix_len(&mut source_file)
        .map_err(|err| lineage_read_error(rollout_path.as_path(), err))?;
    load_opened_source(store, rollout_path, source_file, source_byte_limit).await
}

async fn load_opened_source(
    store: &LocalThreadStore,
    rollout_path: PathBuf,
    source_file: File,
    source_byte_limit: u64,
) -> ThreadStoreResult<StoredForkSource> {
    let (session_meta, source_file) = codex_rollout::read_session_meta_line_from_seekable_prefix(
        rollout_path.as_path(),
        source_file,
        source_byte_limit,
    )
    .await
    .map_err(|err| ThreadStoreError::InvalidRequest {
        message: format!(
            "failed to read fork source metadata {}: {err}",
            rollout_path.display()
        ),
    })?;
    let thread_id = session_meta.meta.id;
    let source_home = source_home_for_rollout_path(rollout_path.as_path());
    if session_meta.meta.history_base.is_some() && source_home.is_none() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "paginated fork source {} has inherited history but is not under a Codex \
                 `sessions` or `archived_sessions` directory",
                rollout_path.display()
            ),
        });
    }
    let mut source_config = store.config.clone();
    if let Some(source_home) = source_home {
        source_config.codex_home = source_home;
    }
    let source_store = LocalThreadStore::new(source_config, /*state_db*/ None);
    let archived = super::helpers::rollout_path_is_archived(
        source_store.config.codex_home.as_path(),
        rollout_path.as_path(),
    );
    let (canonical_meta, items) = if session_meta.meta.history_mode == ThreadHistoryMode::Legacy {
        let (items, loaded_thread_id, parse_errors) =
            codex_rollout::RolloutRecorder::load_rollout_items_from_seekable_prefix(
                rollout_path.as_path(),
                source_file,
                source_byte_limit,
            )
            .await
            .map_err(|err| lineage_read_error(rollout_path.as_path(), err))?;
        if loaded_thread_id != Some(thread_id) {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "fork source {} does not contain metadata for thread {thread_id}",
                    rollout_path.display()
                ),
            });
        }
        if parse_errors != 0 {
            tracing::warn!(
                path = %rollout_path.display(),
                parse_errors,
                "skipped undecodable records while copying legacy fork source"
            );
        }
        (session_meta, items)
    } else {
        let lineage = source_store
            .resolve_rollout_lineage_from_snapshot(
                thread_id,
                rollout_path.clone(),
                session_meta.clone(),
                source_file,
                source_byte_limit,
            )
            .await?;
        load_complete_lineage(lineage, session_meta).await?
    };
    let thread = read_thread::stored_thread_from_meta_line(
        &source_store,
        canonical_meta,
        rollout_path,
        archived,
    );
    Ok(StoredForkSource {
        thread,
        history: StoredThreadHistory { thread_id, items },
    })
}

fn source_home_for_rollout_path(rollout_path: &Path) -> Option<PathBuf> {
    if !rollout_path.is_absolute() {
        return None;
    }
    rollout_path
        .ancestors()
        .find(|ancestor| {
            ancestor.file_name().is_some_and(|name| {
                name == OsStr::new(SESSIONS_SUBDIR) || name == OsStr::new(ARCHIVED_SESSIONS_SUBDIR)
            })
        })
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

async fn load_complete_lineage(
    lineage: OpenedRolloutLineage,
    mut session_meta: SessionMetaLine,
) -> ThreadStoreResult<(SessionMetaLine, Vec<RolloutItem>)> {
    let source_thread_id = session_meta.meta.id;
    let segment_count = lineage.segments.len();
    if segment_count == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "paginated fork source has no rollout lineage".to_string(),
        });
    }
    let mut copied_items = Vec::new();
    for (index, opened_segment) in lineage.segments.into_iter().enumerate() {
        let is_source_segment = index + 1 == segment_count;
        let byte_limit = match opened_segment.segment.end {
            Some(end) => end.end_byte_offset,
            None if is_source_segment => opened_segment.byte_limit,
            None => {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!(
                        "paginated fork ancestor {} has no inherited byte cutoff",
                        opened_segment.segment.rollout_path.display()
                    ),
                });
            }
        };
        let segment_items =
            load_segment_from_snapshot(&opened_segment.segment, opened_segment.file, byte_limit)
                .await?;
        for item in segment_items {
            if let RolloutItem::SessionMeta(meta) = item {
                if is_source_segment && meta.meta.id == source_thread_id {
                    session_meta.meta = meta.meta;
                    if meta.git.is_some() {
                        session_meta.git = meta.git;
                    }
                }
                continue;
            }
            copied_items.push(item);
        }
    }

    let mut items = Vec::with_capacity(copied_items.len() + 1);
    items.push(RolloutItem::SessionMeta(session_meta.clone()));
    items.extend(copied_items);
    Ok((session_meta, items))
}

fn complete_jsonl_prefix_len(file: &mut File) -> std::io::Result<u64> {
    const CHUNK_SIZE: usize = 8 * 1024;

    let file_len = file.metadata()?.len();
    let chunk_size = u64::try_from(CHUNK_SIZE).map_err(std::io::Error::other)?;
    let mut end = file_len;
    let mut buffer = vec![0_u8; CHUNK_SIZE];
    while end != 0 {
        let start = end.saturating_sub(chunk_size);
        let chunk_len = usize::try_from(end - start).map_err(std::io::Error::other)?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..chunk_len])?;
        if let Some(index) = buffer[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            file.seek(SeekFrom::Start(0))?;
            return Ok(start + u64::try_from(index).map_err(std::io::Error::other)? + 1);
        }
        end = start;
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(0)
}

async fn load_segment_from_snapshot(
    segment: &RolloutLineageSegment,
    file: File,
    byte_limit: u64,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let file = tokio::fs::File::from_std(file);
    let mut reader = tokio::io::BufReader::new(file.take(byte_limit));
    let mut expected_ordinal =
        segment
            .start_ordinal()
            .checked_sub(1)
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!(
                    "paginated fork source {} has an invalid starting ordinal",
                    segment.rollout_path.display()
                ),
            })?;
    let mut final_inherited_ordinal = None;
    let mut first_inherited_item_index = None;
    let mut items = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| lineage_read_error(segment.rollout_path.as_path(), err))?;
        if read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "paginated fork source {} cutoff is not at a JSONL record boundary",
                    segment.rollout_path.display()
                ),
            });
        }
        if line.trim().is_empty() {
            continue;
        }
        // A copied fork must account for every physical source record. Silently skipping malformed
        // or forward-unknown records would shift exact rollback indices and make the destination
        // history differ from the auditable source.
        let value =
            serde_json::from_str(&line).map_err(|err| ThreadStoreError::InvalidRequest {
                message: format!(
                    "failed to parse paginated fork source {}: {err}",
                    segment.rollout_path.display()
                ),
            })?;
        let line: RolloutLine = codex_rollout::decode_rollout_line(value).map_err(|err| {
            ThreadStoreError::InvalidRequest {
                message: format!(
                    "failed to decode paginated fork source {}: {err}",
                    segment.rollout_path.display()
                ),
            }
        })?;
        let ordinal = line
            .ordinal
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!(
                    "paginated fork source {} contains a record without an ordinal",
                    segment.rollout_path.display()
                ),
            })?;
        if ordinal != expected_ordinal {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "paginated fork source {} expected ordinal {expected_ordinal}, found \
                     ordinal {ordinal}",
                    segment.rollout_path.display(),
                ),
            });
        }
        if segment
            .end_ordinal()
            .is_some_and(|end_ordinal| ordinal >= end_ordinal)
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "paginated fork source {} byte cutoff includes ordinal {ordinal} beyond \
                     inherited end",
                    segment.rollout_path.display(),
                ),
            });
        }
        expected_ordinal =
            ordinal
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::InvalidRequest {
                    message: format!(
                        "paginated fork source {} contains an ordinal overflow",
                        segment.rollout_path.display()
                    ),
                })?;
        if ordinal >= segment.start_ordinal() {
            first_inherited_item_index.get_or_insert(items.len());
            final_inherited_ordinal = Some(ordinal);
        }
        items.push(line.item);
    }
    if reader.get_ref().limit() != 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "paginated fork source {} ends before its inherited byte cutoff",
                segment.rollout_path.display()
            ),
        });
    }
    if let Some(end_ordinal) = segment.end_ordinal()
        && end_ordinal > segment.start_ordinal()
        && final_inherited_ordinal != end_ordinal.checked_sub(1)
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "paginated fork source {} ends before inherited ordinal {}",
                segment.rollout_path.display(),
                end_ordinal - 1
            ),
        });
    }
    let first_inherited_item_index = first_inherited_item_index.unwrap_or(items.len());
    // Exact rollback indices belong to this physical rollout. Resolve them before concatenating
    // lineage segments, and never let an index-bearing marker escape into the flattened copy.
    let removed = codex_rollout::rollout::exact_rollback_removed_items(&items);
    Ok(items
        .into_iter()
        .zip(removed)
        .skip(first_inherited_item_index)
        .filter(|(item, removed)| {
            !removed
                && !matches!(
                    item,
                    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback))
                        if rollback.rollback_start_index.is_some()
                )
        })
        .map(|(item, _)| item)
        .collect())
}

fn lineage_read_error(path: &Path, err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!(
            "failed to read paginated fork source {}: {err}",
            path.display()
        ),
    }
}

#[cfg(test)]
#[path = "fork_copy_tests.rs"]
mod tests;
