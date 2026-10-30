//! Historical exact markers must be interpreted inside their original frozen source segment.

use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::RolloutItem;

use super::super::rollout_lineage::AncestorReads;
use super::super::rollout_lineage::RolloutLineage;
use super::super::rollout_lineage::RolloutLineageSegment;

pub(super) fn read_segment(
    segment: &RolloutLineageSegment,
    reads: &AncestorReads,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let items = read_canonical_segment_with_reads(segment, reads, is_cancelled)?;
    Ok(codex_rollout::rollout_without_exact_rollback_ranges(&items))
}

pub(in crate::local) fn read_canonical_segment(
    segment: &RolloutLineageSegment,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    read_canonical_segment_with_reads(segment, &AncestorReads::default(), is_cancelled)
}

fn read_canonical_segment_with_reads(
    segment: &RolloutLineageSegment,
    reads: &AncestorReads,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let check_cancelled = || {
        if is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "rollout read cancelled",
            ))
        } else {
            Ok(())
        }
    };
    check_cancelled()?;
    let read = || -> io::Result<Vec<RolloutItem>> {
        let mut file = reads.open(&segment.rollout_path, &is_cancelled)?;
        let actual_length = file.seek(SeekFrom::End(0))?;
        let length = segment.end.map_or(actual_length, |end| end.end_byte_offset);
        if actual_length < length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "frozen rollout prefix is incomplete",
            ));
        }
        file.rewind()?;
        let mut reader = BufReader::new(file.take(length));
        let mut record = Vec::new();
        let mut items = Vec::new();
        loop {
            check_cancelled()?;
            record.clear();
            if reader.read_until(b'\n', &mut record)? == 0 {
                break;
            }
            let line = serde_json::from_slice(&record)
                .ok()
                .and_then(|value| codex_rollout::decode_canonical_rollout_line(value).ok())
                .flatten();
            if let Some(line) = line {
                items.push(line.item);
            }
        }
        if reader.get_ref().limit() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "frozen rollout prefix shortened during read",
            ));
        }
        Ok(items)
    };
    read().inspect_err(|error| {
        if error.kind() != io::ErrorKind::Interrupted {
            reads.record_failure(&segment.rollout_path);
        }
    })
}

pub(in crate::local) fn load_full_lineage(
    lineage: &RolloutLineage,
    session_meta: SessionMetaLine,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let mut items = vec![RolloutItem::SessionMeta(Box::new(session_meta))];
    items.extend(read_full_lineage(lineage, is_cancelled)?);
    Ok(items)
}

/// Return context records only; the caller installs its captured leaf metadata once.
/// Keep rollback masking inside each frozen source segment before removing its header.
pub(super) fn read_full_lineage(
    lineage: &RolloutLineage,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let mut items = Vec::new();
    for segment in lineage.segments() {
        // Mask before replacing even copied SessionMeta records: they occupied raw indexes too.
        items.extend(
            read_segment(segment, &lineage.reads, &is_cancelled)?
                .into_iter()
                .filter(|item| !matches!(item, RolloutItem::SessionMeta(_))),
        );
    }
    Ok(items)
}
