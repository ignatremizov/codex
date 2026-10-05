//! Historical exact markers must be interpreted inside their original frozen source segment.

use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::RolloutItem;

use super::super::rollout_lineage::RolloutLineage;
use super::super::rollout_lineage::RolloutLineageSegment;

pub(super) fn read_segment(
    segment: &RolloutLineageSegment,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let mut file =
        codex_rollout::open_rollout_seekable_reader(&segment.rollout_path, is_cancelled)?;
    let length = match segment.end {
        Some(end) => end.end_byte_offset,
        None => file.seek(SeekFrom::End(0))?,
    };
    file.rewind()?;
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
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let mut items = vec![RolloutItem::SessionMeta(Box::new(session_meta))];
    for segment in lineage.segments() {
        // Mask before replacing even copied SessionMeta records: they occupied raw indexes too.
        items.extend(
            read_segment(segment, &is_cancelled)?
                .into_iter()
                .filter(|item| !matches!(item, RolloutItem::SessionMeta(_))),
        );
    }
    Ok(items)
}
