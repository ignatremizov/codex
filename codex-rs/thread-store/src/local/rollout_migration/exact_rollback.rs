//! Translate exact rollback coordinates before migration normalization changes record indices.
//!
//! Canonical decoding owns the coordinate space, including copied metadata and retired events.
//! Compatibility-only records have no canonical index; their physical source positions determine
//! whether they fall inside an exact interval. No files or projections are modified here until
//! the complete mapping and subsequent count-only plan have been validated under writer exclusion.

use std::io::BufRead;
use std::path::Path;

use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use tokio::io::AsyncWriteExt;
use tokio::io::BufWriter;

use super::CanonicalizationSource;
use super::LegacyRolloutCanonicalizer;
use super::PROJECTION_BATCH_BYTES;
use super::RollbackPlanner;
use super::RolloutMigrationRateLimiter;
use super::line_parser;
use super::migration_error;
use crate::ThreadStoreResult;

pub(super) async fn prepare(
    path: &Path,
    limiter: &mut RolloutMigrationRateLimiter,
) -> ThreadStoreResult<Option<Vec<RolloutLine>>> {
    // Detection remains streaming, including for compressed sources. Unlike migration's capped
    // compatibility reader, this reader cannot silently miss a valid oversized exact marker.
    // Inspect bytes rather than UTF-8 lines: count-only migration historically skips invalid
    // UTF-8 records. An exact source still must satisfy the full canonical reader below.
    let detection_path = path.to_path_buf();
    let (has_exact, byte_count) = tokio::task::spawn_blocking(move || {
        let source = codex_rollout::open_rollout_seekable_reader(&detection_path)?;
        let mut reader = std::io::BufReader::new(source);
        let mut raw = Vec::new();
        let mut byte_count = 0u64;
        loop {
            raw.clear();
            let read = reader.read_until(b'\n', &mut raw)?;
            if read == 0 {
                return Ok::<_, std::io::Error>((false, byte_count));
            }
            byte_count = byte_count.saturating_add(read as u64);
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw)
                && value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg")
                && value["payload"]["type"].as_str() == Some("thread_rolled_back")
                && !value["payload"]["rollback_start_index"].is_null()
            {
                return Ok((true, byte_count));
            }
        }
    })
    .await
    .map_err(migration_error)?
    .map_err(migration_error)?;
    limiter.account(byte_count).await;
    if !has_exact {
        return Ok(None);
    }
    let mut reader = codex_rollout::open_rollout_line_reader(path)
        .await
        .map_err(migration_error)?;
    let mut canonical = Vec::new();
    let mut positions = Vec::new();
    let mut records = Vec::new();
    let mut source_position = 0usize;
    while let Some(raw) = reader.next_line().await.map_err(migration_error)? {
        limiter.account(raw.len() as u64).await;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            let exact_marker = value.get("type").and_then(serde_json::Value::as_str)
                == Some("event_msg")
                && value["payload"]["type"].as_str() == Some("thread_rolled_back")
                && !value["payload"]["rollback_start_index"].is_null();
            let decoded = codex_rollout::decode_canonical_rollout_line(value.clone())
                .ok()
                .flatten();
            if exact_marker && decoded.is_none() {
                return Err(migration_error(
                    "exact rollback marker has no canonical coordinate",
                ));
            }
            if let Some(line) = decoded {
                let index = canonical.len();
                canonical.push(line.item.clone());
                positions.push(source_position);
                records.push((source_position, Some(index), line));
            } else if let Ok(Some(line)) = line_parser::parse_legacy_rollout_value(value) {
                records.push((source_position, None, line));
            }
        }
        source_position = source_position
            .checked_add(1)
            .ok_or_else(|| migration_error("exact rollback source position overflow"))?;
    }
    select(canonical, positions, records).map(Some)
}

fn select(
    canonical: Vec<RolloutItem>,
    positions: Vec<usize>,
    records: Vec<(usize, Option<usize>, RolloutLine)>,
) -> ThreadStoreResult<Vec<RolloutLine>> {
    let mut provenance = super::context_provenance::ContextProvenance::default();
    for item in &canonical {
        provenance.observe(item);
    }
    let proofs = provenance.finish();
    let mut intervals = Vec::new();
    for (index, item) in canonical.iter().enumerate() {
        if let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) = item
            && let Some(start) = event.rollback_start_index
        {
            let start = usize::try_from(start).map_err(|_| {
                migration_error("exact rollback cutoff exceeds addressable history")
            })?;
            if start >= index {
                return Err(migration_error(
                    "exact rollback cutoff must precede its marker",
                ));
            }
            intervals.push((positions[start], positions[index]));
        }
    }
    let removed = codex_rollout::exact_rollback_removed_items(&canonical);
    let mut lines = Vec::new();
    for (position, canonical_index, line) in records {
        let is_removed = canonical_index.map_or_else(
            || {
                intervals
                    .iter()
                    .any(|(start, end)| *start <= position && position < *end)
            },
            |index| removed[index],
        );
        if is_removed
            || matches!(&line.item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event))
                if event.rollback_start_index.is_some())
        {
            continue;
        }
        lines.push(line);
    }
    // Exact ranges have already been applied. Only surviving count-only markers reach either
    // forward ownership replay or reverse compaction replay, so mixed histories do not double pop.
    let mut planner = RollbackPlanner::new(proofs);
    for line in &lines {
        planner.observe(line)?;
    }
    let plan = planner.finish();
    lines
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| plan.apply(index, line).transpose())
        .collect()
}

pub(super) async fn write(
    input: &CanonicalizationSource<'_>,
    lines: Vec<RolloutLine>,
    limiter: &mut RolloutMigrationRateLimiter,
) -> ThreadStoreResult<(u64, u64)> {
    let staged_file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(input.staged_path)
        .await
        .map_err(migration_error)?;
    staged_file
        .set_permissions(input.source_permissions.clone())
        .await
        .map_err(migration_error)?;
    let mut staged = BufWriter::with_capacity(PROJECTION_BATCH_BYTES as usize, staged_file);
    let mut canonicalizer = LegacyRolloutCanonicalizer::new(input.thread_id);
    limiter
        .account(
            canonicalizer
                .write_head_session_meta(input.canonical_session_meta.clone(), &mut staged)
                .await?,
        )
        .await;
    let mut last_timestamp = input.canonical_session_meta.timestamp.clone();
    for line in lines {
        last_timestamp = line.timestamp.clone();
        limiter
            .account(canonicalizer.process_line(line, &mut staged).await?)
            .await;
    }
    limiter
        .account(canonicalizer.finish(&mut staged, &last_timestamp).await?)
        .await;
    staged.flush().await.map_err(migration_error)?;
    Ok((
        canonicalizer.output_byte_offset(),
        canonicalizer.next_ordinal(),
    ))
}

#[cfg(test)]
#[path = "exact_rollback_tests.rs"]
mod tests;
