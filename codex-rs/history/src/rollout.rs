use std::collections::HashMap;

use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::is_user_agent_task_context_response_item_id;

use crate::RolloutItem;

/// Marks raw rollout items removed by exact rollback markers.
///
/// A marker's cutoff is an absolute index in the same raw rollout. The marker and items from that
/// cutoff through the marker are removed together, except that a terminal event is retained when
/// its matching turn start survives the range and durable out-of-band agent artifacts are retained
/// regardless of their position. These include accepted subagent responses, their presentation
/// state, trusted user-agent task links, and source-side user-agent control audit. A terminal
/// `wait_agent` item is one such artifact when that durable item owns completion presentation
/// instead of a background row.
/// Computing all ranges before replay also ensures that rollback markers inside a newer removed
/// range cannot affect the surviving history.
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

    // Subagent completion context and presentation are out-of-band arrivals, not output owned by
    // the user turn whose raw range happens to contain them. Once accepted and durably appended,
    // later exact rollback must not erase them. Preserve inter-agent delivery metadata immediately
    // preceding a committed response item as part of the same durable pair.
    for index in 0..items.len() {
        if !removed[index] || !is_durable_agent_artifact(items, index) {
            continue;
        }
        removed[index] = false;
        if matches!(&items[index], RolloutItem::ResponseItem(_))
            && matches!(
                index.checked_sub(1).and_then(|index| items.get(index)),
                Some(RolloutItem::InterAgentCommunicationMetadata { .. })
            )
        {
            removed[index - 1] = false;
        }
    }
    removed
}

fn is_durable_agent_artifact(items: &[RolloutItem], index: usize) -> bool {
    match &items[index] {
        RolloutItem::ResponseItem(item) => {
            let Some(response_item_id) = item.id() else {
                return false;
            };
            if is_user_agent_task_context_response_item_id(response_item_id.as_str()) {
                return true;
            }
            let reserved_completion_context =
                is_sub_agent_completion_context_response_item_id(response_item_id.as_str())
                    && matches!(
                        index.checked_sub(1).and_then(|index| items.get(index)),
                        Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                    );
            let committed_observed_response = items
                .iter()
                .skip(index.saturating_add(1))
                .take_while(|item| matches!(item, RolloutItem::AgentResponseObservation(_)))
                .any(|item| {
                    matches!(
                        item,
                        RolloutItem::AgentResponseObservation(observation)
                            if observation
                                .committed_delivery_response_item_ids
                                .contains(response_item_id)
                    )
                });
            reserved_completion_context || committed_observed_response
        }
        RolloutItem::AgentResponseObservation(_) => true,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
            event.item.is_sub_agent_completion_presentation()
                || matches!(&event.item, TurnItem::UserAgentControl(_))
        }
        RolloutItem::SessionMeta(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::EventMsg(_) => false,
    }
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
