//! Exact rollback coordinates are decoded source-record indexes, independent of ordinals.

use std::collections::HashSet;

use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;

pub(super) struct ExactRollbackProjection {
    pub(super) removed_offsets: HashSet<u64>,
    pub(super) contains_exact_marker: bool,
}

impl ExactRollbackProjection {
    pub(super) fn read(bytes: &[u8], start_offset: u64) -> Self {
        let mut items = Vec::new();
        let mut offsets = Vec::new();
        let mut offset = start_offset;
        let mut contains_exact_marker = false;
        for record in bytes.split_inclusive(|byte| *byte == b'\n') {
            let line = serde_json::from_slice(record)
                .ok()
                .and_then(|value| codex_rollout::decode_canonical_rollout_line(value).ok())
                .flatten();
            if let Some(line) = line {
                contains_exact_marker |= matches!(
                    &line.item,
                    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event))
                        if event.rollback_start_index.is_some()
                );
                offsets.push(offset);
                items.push(line.item);
            }
            offset += record.len() as u64;
        }
        // A suffix's local indexes are not canonical coordinates. Its marker only requests a
        // rebuild; the full source pass computes the mask before any projection filtering.
        let removed_offsets = if start_offset == 0 && contains_exact_marker {
            offsets
                .into_iter()
                .zip(codex_rollout::exact_rollback_removed_items(&items))
                .filter_map(|(offset, removed)| removed.then_some(offset))
                .collect()
        } else {
            HashSet::new()
        };
        Self {
            removed_offsets,
            contains_exact_marker,
        }
    }
}
