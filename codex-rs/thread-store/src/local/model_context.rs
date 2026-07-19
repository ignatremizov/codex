use std::fs::File;
use std::io;
use std::path::PathBuf;

use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::ScanOutcome;

use super::LocalThreadStore;
use super::read_thread;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;

/// Loads rollout items needed to reconstruct the latest model-visible context.
///
/// Paginated JSONL rollouts use a reverse scan. Compressed input is streamed into a temporary
/// seekable file first, without changing the canonical parent representation. When the scan finds
/// both a usable replacement-history checkpoint and the completed user-turn context needed for
/// resume metadata, the returned replay starts with the canonical head `SessionMeta` followed by
/// that newest suffix. When no bounded cutoff is available, the scan continues to the beginning and
/// returns the complete replay it already accumulated.
///
/// Legacy rollout shapes keep the existing full-history path.
pub(super) async fn load_latest_model_context(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    let path = read_thread::resolve_rollout_path(store, params.thread_id, params.include_archived)
        .await?
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {}", params.thread_id),
        })?;

    let session_meta = codex_rollout::read_session_meta_line(path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read session metadata {}: {err}", path.display()),
        })?;
    if session_meta.meta.id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout at {} belongs to thread {}, not {}",
                path.display(),
                session_meta.meta.id,
                params.thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        scan_model_context_from_end(path, session_meta).await?
    } else {
        read_thread::load_history_items(path.as_path()).await?
    };

    Ok(StoredModelContext {
        thread_id: params.thread_id,
        items,
    })
}

async fn scan_model_context_from_end(
    path: PathBuf,
    session_meta: SessionMetaLine,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let reader = codex_rollout::open_rollout_seekable_reader(path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to open model context {}: {err}", path.display()),
        })?;
    tokio::task::spawn_blocking(move || scan_model_context_from_end_blocking(reader, session_meta))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to join model context scan: {err}"),
        })?
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to scan model context {}: {err}", path.display()),
        })
}

fn scan_model_context_from_end_blocking(
    reader: File,
    session_meta: SessionMetaLine,
) -> io::Result<Vec<RolloutItem>> {
    let mut scan = ModelContextScan::default();
    let mut scanner = ReverseJsonlScanner::new(reader)?;
    while let Some(outcome) = scanner.scan_next::<RolloutLine>()? {
        let ScanOutcome::Parsed(line) = outcome else {
            continue;
        };
        match scan.push(line.item) {
            ModelContextScanProgress::Continue => {}
            ModelContextScanProgress::Complete => break,
        }
    }

    Ok(scan.finish(session_meta))
}
