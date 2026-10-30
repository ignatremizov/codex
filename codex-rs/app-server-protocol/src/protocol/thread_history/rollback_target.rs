//! Resolve prompt cuts in canonical decoded coordinates, including mid-turn user steers.

use super::ThreadHistoryBuilder;
use super::ThreadItem;
use super::exact_rollback_removed_items;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_rollout::UserPromptSource;

/// A materialized suffix boundary in the original canonical decoded rollout coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedRollbackStart {
    pub rollout_index: usize,
    pub turn_id: String,
    pub turn_count: usize,
}

/// Returns the selected turn's opening boundary, or the exact selected user-message boundary.
///
/// Item identity is resolved against the complete materialized turn before replaying its source
/// coordinates. Equal text, clipped history, and already-removed items cannot select another cut.
pub fn materialized_rollback_start(
    items: &[RolloutItem],
    num_turns: u32,
    before_item_id: Option<&str>,
) -> Option<MaterializedRollbackStart> {
    let removed = exact_rollback_removed_items(items);
    let mut builder = ThreadHistoryBuilder::new();
    for (item, removed) in items.iter().zip(&removed) {
        if !removed {
            builder.handle_rollout_item(item);
        } else {
            builder.skip_rollout_item();
        }
    }
    builder.finish_current_turn();
    let count = usize::try_from(num_turns).ok().filter(|count| *count > 0)?;
    let start = builder.turns.len().checked_sub(count)?;
    let turn = builder.turns.get(start)?;
    let mut boundary = MaterializedRollbackStart {
        rollout_index: turn.rollout_start_index,
        turn_id: turn.id.clone(),
        turn_count: builder.turns.len(),
    };
    let Some(item_id) = before_item_id else {
        return Some(boundary);
    };
    let mut replay = ThreadHistoryBuilder::new();
    let mut source = UserPromptSource::default();
    let mut presentations = Vec::new();
    let mut selected = None;
    let mut canonical_presentations = Vec::new();
    let mut canonical_selected = None;
    let end = builder
        .turns
        .get(start + 1)
        .map_or(items.len(), |turn| turn.rollout_start_index);
    for (index, (item, removed)) in items.iter().zip(removed).enumerate() {
        if removed {
            replay.skip_rollout_item();
            continue;
        }
        let changes = replay.handle_rollout_item_with_changes(item);
        if index < boundary.rollout_index || index >= end {
            continue;
        }
        source.observe(item, index);
        if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item
            && event.turn_id == boundary.turn_id
            && let TurnItem::UserMessage(user) = &event.item
        {
            if user.id == item_id {
                if canonical_selected.is_some() {
                    return None;
                }
                canonical_selected = Some(canonical_presentations.len());
            }
            canonical_presentations.push(index);
        }
        if !matches!(item, RolloutItem::EventMsg(EventMsg::UserMessage(_))) {
            continue;
        }
        for change in changes.changed_items {
            if let ThreadItem::UserMessage { id, .. } = change.item {
                if id == item_id {
                    if selected.is_some() {
                        return None;
                    }
                    selected = Some(presentations.len());
                }
                presentations.push(index);
            }
        }
    }
    let (presentations, selected) = match (selected, canonical_selected) {
        (Some(selected), None) => {
            if !turn.items.iter().any(|item| {
                matches!(
                    item, ThreadItem::UserMessage { id, .. } if id == item_id
                )
            }) {
                return None;
            }
            (presentations, selected)
        }
        (None, Some(selected)) => (canonical_presentations, selected),
        (None, None) | (Some(_), Some(_)) => return None,
    };
    if selected == 0 {
        return Some(boundary);
    }
    boundary.rollout_index = source.boundary(&presentations, selected)?;
    Some(boundary)
}

#[cfg(test)]
#[path = "rollback_target_tests.rs"]
mod tests;
