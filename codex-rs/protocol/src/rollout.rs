use std::collections::HashMap;

use crate::protocol::EventMsg;
use crate::protocol::RolloutItem;

/// Marks raw rollout items removed by exact rollback markers.
///
/// A marker's cutoff is an absolute index in the same raw rollout. The marker and items from that
/// cutoff through the marker are removed together, except that a terminal event is retained when
/// its matching turn start survives the range. Computing all ranges before replay also ensures
/// that rollback markers inside a newer removed range cannot affect the surviving history.
pub fn exact_rollback_removed_items(items: &[RolloutItem]) -> Vec<bool> {
    let mut range_starts = vec![0_usize; items.len().saturating_add(1)];
    let mut range_ends = vec![0_usize; items.len().saturating_add(1)];
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
        range_starts[start_index] = range_starts[start_index].saturating_add(1);
        range_ends[marker_index.saturating_add(1)] =
            range_ends[marker_index.saturating_add(1)].saturating_add(1);
    }

    let mut active_ranges = 0_usize;
    let mut removed = range_starts
        .into_iter()
        .zip(range_ends)
        .take(items.len())
        .map(|(starts, ends)| {
            active_ranges = active_ranges.saturating_sub(ends).saturating_add(starts);
            active_ranges > 0
        })
        .collect::<Vec<_>>();

    // Rolling back a steer can cut through the middle of an explicit turn. Keep the persisted
    // terminal event when that turn's start survives so cold replay does not resurrect the
    // retained prefix as an in-progress turn.
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
                    if active_turn_start
                        .is_some_and(|(_, active_index)| active_index == start_index)
                    {
                        active_turn_start = None;
                    }
                    if !removed[start_index] {
                        removed[index] = false;
                    }
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                let start = match event.turn_id.as_deref() {
                    Some(turn_id) => turn_starts
                        .remove(turn_id)
                        .map(|start_index| (turn_id, start_index)),
                    None => active_turn_start.take(),
                };
                if let Some((turn_id, start_index)) = start {
                    turn_starts.remove(turn_id);
                    if active_turn_start
                        .is_some_and(|(_, active_index)| active_index == start_index)
                    {
                        active_turn_start = None;
                    }
                    if !removed[start_index] {
                        removed[index] = false;
                    }
                }
            }
            _ => {}
        }
    }
    removed
}

/// Returns the effective raw rollout with exact rollback ranges and their markers removed.
///
/// Call this before copying or filtering a rollout so absolute rollback cutoffs never escape into
/// a transformed index space.
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
