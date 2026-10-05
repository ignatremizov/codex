use std::collections::HashMap;

use codex_protocol::protocol::EventMsg;

use crate::RolloutItem;

/// Computes the cumulative mask of raw rollout records removed by exact rollback markers.
///
/// Exact ranges are absolute positions in the decoded canonical rollout. Overlapping ranges are
/// unioned: a later rollback never resurrects records removed by an earlier one. Callers that
/// need exact semantics must retain a decoded canonical vector and its source-position mapping;
/// normalized or concatenated history is not a valid coordinate space.
pub fn exact_rollback_removed_items(items: &[RolloutItem]) -> Vec<bool> {
    let mut starts = vec![0usize; items.len().saturating_add(1)];
    let mut ends = vec![0usize; items.len().saturating_add(1)];
    for (marker_index, item) in items.iter().enumerate() {
        let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) = item else {
            continue;
        };
        let Some(start_index) = rollback
            .rollback_start_index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|start_index| *start_index < marker_index)
        else {
            continue;
        };
        starts[start_index] = starts[start_index].saturating_add(1);
        ends[marker_index.saturating_add(1)] =
            ends[marker_index.saturating_add(1)].saturating_add(1);
    }

    let mut active_ranges = 0usize;
    let mut removed = starts
        .into_iter()
        .zip(ends)
        .take(items.len())
        .map(|(start, end)| {
            active_ranges = active_ranges.saturating_sub(end).saturating_add(start);
            active_ranges > 0
        })
        .collect::<Vec<_>>();

    // Keep terminal evidence for a surviving explicit turn. This prevents replay from reviving
    // the retained prefix as an in-progress turn when a rollback cuts through that turn.
    let mut turn_starts = HashMap::new();
    let mut active_turn_start = None;
    for (index, item) in items.iter().enumerate() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                turn_starts.insert(event.turn_id.as_str(), index);
                active_turn_start = Some((event.turn_id.as_str(), index));
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                if let Some(start_index) = turn_starts.remove(event.turn_id.as_str()) {
                    if !removed[start_index] {
                        removed[index] = false;
                    }
                    if active_turn_start.is_some_and(|(_, active)| active == start_index) {
                        active_turn_start = None;
                    }
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                let start = match event.turn_id.as_deref() {
                    Some(turn_id) => turn_starts.remove(turn_id),
                    None => active_turn_start.and_then(|(turn_id, _)| turn_starts.remove(turn_id)),
                };
                if let Some(start_index) = start {
                    if !removed[start_index] {
                        removed[index] = false;
                    }
                    if active_turn_start.is_some_and(|(_, active)| active == start_index) {
                        active_turn_start = None;
                    }
                }
            }
            _ => {}
        }
    }
    removed
}

/// Returns a canonical rollout with exact rollback ranges and marker records removed.
pub fn rollout_without_exact_rollback_ranges(items: &[RolloutItem]) -> Vec<RolloutItem> {
    items
        .iter()
        .zip(exact_rollback_removed_items(items))
        .filter(|(_, removed)| !removed)
        .map(|(item, _)| item.clone())
        .collect()
}

#[cfg(test)]
#[path = "rollout_tests.rs"]
mod tests;
