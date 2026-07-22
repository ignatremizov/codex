//! Helpers for truncating rollouts based on "user turn" boundaries.
//!
//! In core, "user turns" are detected by scanning `ResponseItem::Message` items and
//! interpreting them via `event_mapping::parse_turn_item(...)`.

use crate::context_manager::is_user_turn_boundary;
use crate::event_mapping;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::build_turns_from_rollout_items;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::rollout::exact_rollback_removed_items;
use codex_protocol::rollout::rollout_without_exact_rollback_ranges;
use std::ops::Range;

pub(crate) fn initial_history_has_prior_user_turns(conversation_history: &InitialHistory) -> bool {
    !instruction_positions_in_rollout(conversation_history.get_rollout_items()).is_empty()
}

/// Return effective model-visible instruction boundary positions in a rollout.
///
/// Exact rollback markers remove boundaries at or after their persisted cutoff. Legacy markers
/// retain their historical count-from-the-end behavior.
pub(crate) fn instruction_positions_in_rollout(items: &[RolloutItem]) -> Vec<usize> {
    let mut instruction_positions = Vec::new();
    let mut pending_user_event_index = None;
    let exact_rollback_removals = exact_rollback_removed_items(items);
    for (idx, item) in items.iter().enumerate() {
        if exact_rollback_removals[idx] {
            pending_user_event_index = None;
            continue;
        }
        match item {
            RolloutItem::ResponseItem(item) if is_user_turn_boundary(item) => {
                let boundary_index = if is_real_user_message_boundary(item) {
                    pending_user_event_index.take().unwrap_or(idx)
                } else if matches!(item, ResponseItem::AgentMessage { .. })
                    && idx.checked_sub(1).is_some_and(|previous_idx| {
                        !exact_rollback_removals[previous_idx]
                            && matches!(
                                items.get(previous_idx),
                                Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                            )
                    })
                {
                    idx.saturating_sub(1)
                } else {
                    idx
                };
                instruction_positions.push(boundary_index);
            }
            RolloutItem::InterAgentCommunication(_) => {
                pending_user_event_index = None;
                instruction_positions.push(idx);
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                pending_user_event_index = Some(idx);
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                pending_user_event_index = None;
                if rollback.rollback_start_index.is_none() {
                    let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                    instruction_positions
                        .truncate(instruction_positions.len().saturating_sub(num_turns));
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) => {
                pending_user_event_index = None;
            }
            _ => {}
        }
    }
    instruction_positions
}

/// Return the indices of user message boundaries in a rollout.
///
/// A user message boundary is a `RolloutItem::ResponseItem(ResponseItem::Message { .. })`
/// whose parsed turn item is `TurnItem::UserMessage`.
///
/// Rollouts can contain `ThreadRolledBack` markers. Those markers indicate that the
/// last N user turns were removed from the effective thread history; we apply them here so
/// indexing uses the post-rollback history rather than the raw stream.
pub(crate) fn user_message_positions_in_rollout(items: &[RolloutItem]) -> Vec<usize> {
    let mut user_positions = Vec::new();
    let exact_rollback_removals = exact_rollback_removed_items(items);
    for (idx, item) in items.iter().enumerate() {
        if exact_rollback_removals[idx] {
            continue;
        }
        match item {
            RolloutItem::ResponseItem(item @ ResponseItem::Message { .. })
                if matches!(
                    event_mapping::parse_turn_item(item),
                    Some(TurnItem::UserMessage(_))
                ) =>
            {
                user_positions.push(idx);
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback))
                if rollback.rollback_start_index.is_none() =>
            {
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                user_positions.truncate(user_positions.len().saturating_sub(num_turns));
            }
            _ => {}
        }
    }
    user_positions
}

/// Return the indices of fork-turn boundaries in a rollout.
///
/// A fork-turn boundary is either:
/// - a real user message boundary, or
/// - an inter-agent communication whose `trigger_turn` is `true`, or
/// - a legacy assistant inter-agent envelope with the same flag.
///
/// Like `user_message_positions_in_rollout`, this applies `ThreadRolledBack` markers so indexing
/// reflects the effective post-rollback history. Rollback counts instruction turns, so a rollback
/// removes the stale suffix starting at the earliest rolled-back instruction-turn boundary instead
/// of simply truncating the mixed fork-boundary list.
pub(crate) fn fork_turn_positions_in_rollout(items: &[RolloutItem]) -> Vec<usize> {
    let mut rollback_turn_positions = Vec::new();
    let mut fork_turn_positions = Vec::new();
    let exact_rollback_removals = exact_rollback_removed_items(items);
    for (idx, item) in items.iter().enumerate() {
        if exact_rollback_removals[idx] {
            continue;
        }
        match item {
            RolloutItem::ResponseItem(item) => {
                let has_delivery_metadata = matches!(item, ResponseItem::AgentMessage { .. })
                    && idx.checked_sub(1).is_some_and(|previous_idx| {
                        matches!(
                            items.get(previous_idx),
                            Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                        )
                    });
                if is_user_turn_boundary(item) && !has_delivery_metadata {
                    rollback_turn_positions.push(idx);
                }
                if is_real_user_message_boundary(item) || is_trigger_turn_boundary(item) {
                    fork_turn_positions.push(idx);
                }
            }
            RolloutItem::InterAgentCommunication(communication) => {
                rollback_turn_positions.push(idx);
                if communication.trigger_turn {
                    fork_turn_positions.push(idx);
                }
            }
            RolloutItem::InterAgentCommunicationMetadata { trigger_turn } => {
                rollback_turn_positions.push(idx);
                if *trigger_turn {
                    fork_turn_positions.push(idx);
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                if rollback.rollback_start_index.is_some() {
                    continue;
                }
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                if num_turns == 0 {
                    continue;
                }
                let Some(rollback_start_idx) = rollback_turn_positions
                    .len()
                    .checked_sub(num_turns)
                    .map(|rollback_start| rollback_turn_positions[rollback_start])
                    .or_else(|| rollback_turn_positions.first().copied())
                else {
                    continue;
                };
                rollback_turn_positions.retain(|position| *position < rollback_start_idx);
                fork_turn_positions.retain(|position| *position < rollback_start_idx);
            }
            _ => {}
        }
    }
    fork_turn_positions
}

/// Return a prefix of `items` obtained by cutting strictly before the nth user message.
///
/// The boundary index is 0-based from the start of `items` (so `n_from_start = 0` returns
/// a prefix that excludes the first user message and everything after it).
///
/// If `n_from_start` is `usize::MAX`, this returns the full rollout (no truncation).
/// If fewer than or equal to `n_from_start` user messages exist, this returns the full
/// rollout unchanged.
pub(crate) fn truncate_rollout_before_nth_user_message_from_start(
    items: &[RolloutItem],
    n_from_start: usize,
) -> Vec<RolloutItem> {
    if n_from_start == usize::MAX {
        return rollout_without_exact_rollback_ranges(items);
    }

    let user_positions = user_message_positions_in_rollout(items);

    // If fewer than or equal to n user messages exist, keep the full rollout.
    if user_positions.len() <= n_from_start {
        return rollout_without_exact_rollback_ranges(items);
    }

    // Cut strictly before the nth user message (do not keep the nth itself).
    let cut_idx = user_positions[n_from_start];
    exact_rollback_filtered_slice(items, 0..cut_idx)
}

/// Return a rollout prefix ending after the requested persisted terminal turn.
///
/// The turn must still be present in the effective post-rollback history and
/// must have an explicit persisted TurnStarted boundary. Synthetic IDs
/// generated while projecting legacy rollouts are intentionally unsupported
/// because they do not provide a stable raw rollout boundary for a fork.
pub fn truncate_rollout_after_turn_id(
    items: &[RolloutItem],
    last_turn_id: &str,
) -> CodexResult<Vec<RolloutItem>> {
    let turns = build_turns_from_rollout_items(items);
    let turn = turns
        .iter()
        .find(|turn| turn.id == last_turn_id)
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' was not found in the source thread"
            ))
        })?;

    let exact_rollback_removals = exact_rollback_removed_items(items);
    let target_start_index = items
        .iter()
        .enumerate()
        .find_map(|(index, item)| {
            (!exact_rollback_removals[index]
                && matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                    if event.turn_id == last_turn_id
                ))
            .then_some(index)
        })
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' is not a persisted canonical turn in the source thread"
            ))
        })?;

    if matches!(turn.status, TurnStatus::InProgress) {
        return Err(CodexErr::InvalidRequest(format!(
            "lastTurnId '{last_turn_id}' identifies an in-progress turn"
        )));
    }

    let cut_index = items
        .iter()
        .enumerate()
        .skip(target_start_index.saturating_add(1))
        .find_map(|(index, item)| {
            (!exact_rollback_removals[index]
                && matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
            .then_some(index)
        })
        .unwrap_or(items.len());
    Ok(exact_rollback_filtered_slice(items, 0..cut_index))
}

/// Return a rollout prefix ending immediately before the requested persisted turn.
pub fn truncate_rollout_before_turn_id(
    items: &[RolloutItem],
    before_turn_id: &str,
) -> CodexResult<Vec<RolloutItem>> {
    let turns = build_turns_from_rollout_items(items);
    if !turns.iter().any(|turn| turn.id == before_turn_id) {
        return Err(CodexErr::InvalidRequest(format!(
            "beforeTurnId '{before_turn_id}' was not found in the source thread"
        )));
    }

    let exact_rollback_removals = exact_rollback_removed_items(items);
    let cut_index = items
        .iter()
        .enumerate()
        .find_map(|(index, item)| {
            (!exact_rollback_removals[index]
                && matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                    if event.turn_id == before_turn_id
                ))
            .then_some(index)
        })
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "beforeTurnId '{before_turn_id}' is not a persisted canonical turn in the source thread"
            ))
        })?;
    Ok(exact_rollback_filtered_slice(items, 0..cut_index))
}

/// Return a suffix of `items` that keeps the last `n_from_end` fork turns.
///
/// If fewer than or equal to `n_from_end` fork turns exist, this keeps from the first fork-turn
/// boundary and still drops pre-turn startup context.
pub(crate) fn truncate_rollout_to_last_n_fork_turns(
    items: &[RolloutItem],
    n_from_end: usize,
) -> Vec<RolloutItem> {
    if n_from_end == 0 {
        return Vec::new();
    }

    let fork_turn_positions = fork_turn_positions_in_rollout(items);
    let Some(keep_idx) = fork_turn_positions
        .len()
        .checked_sub(n_from_end)
        .map(|position| fork_turn_positions[position])
        .or_else(|| fork_turn_positions.first().copied())
    else {
        return Vec::new();
    };
    exact_rollback_filtered_slice(items, keep_idx..items.len())
}

fn exact_rollback_filtered_slice(items: &[RolloutItem], range: Range<usize>) -> Vec<RolloutItem> {
    items
        .iter()
        .zip(exact_rollback_removed_items(items))
        .enumerate()
        .filter(|(index, (_, removed))| range.contains(index) && !removed)
        .map(|(_, (item, _))| item.clone())
        .collect()
}

fn is_real_user_message_boundary(item: &ResponseItem) -> bool {
    matches!(
        event_mapping::parse_turn_item(item),
        Some(TurnItem::UserMessage(_))
    )
}

fn is_trigger_turn_boundary(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    role == "assistant"
        && InterAgentCommunication::from_message_content(content)
            .is_some_and(|communication| communication.trigger_turn)
}

#[cfg(test)]
#[path = "thread_rollout_truncation_tests.rs"]
mod tests;
