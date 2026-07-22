//! Historical exact markers must be interpreted inside their original frozen source segment.

use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;

use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::RolloutItem;

use super::super::rollout_lineage::RolloutLineage;
use super::super::rollout_lineage::RolloutLineageSegment;

pub(super) fn read_segment(segment: &RolloutLineageSegment) -> io::Result<Vec<RolloutItem>> {
    let file = codex_rollout::open_rollout_seekable_reader(&segment.rollout_path)?;
    let length = segment
        .end
        .map_or(file.metadata()?.len(), |end| end.end_byte_offset);
    let reader = BufReader::new(file.take(length));
    let mut items = Vec::new();
    for record in reader.split(b'\n') {
        let record = record?;
        let line = serde_json::from_slice(&record)
            .ok()
            .and_then(|value| codex_rollout::decode_canonical_rollout_line(value).ok())
            .flatten();
        if let Some(line) = line {
            items.push(line.item);
        }
    }
    Ok(codex_rollout::rollout_without_exact_rollback_ranges(&items))
}

pub(super) fn load_full_lineage(
    lineage: &RolloutLineage,
    session_meta: SessionMetaLine,
) -> io::Result<Vec<RolloutItem>> {
    let mut items = vec![RolloutItem::SessionMeta(session_meta)];
    for segment in lineage.segments() {
        // Mask before replacing even copied SessionMeta records: they occupied raw indexes too.
        items.extend(
            read_segment(segment)?
                .into_iter()
                .filter(|item| !matches!(item, RolloutItem::SessionMeta(_))),
        );
    }
    Ok(items)
}
