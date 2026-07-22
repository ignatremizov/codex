//! Materialize the latest thread turns from a persisted snapshot and buffered lifecycle events.

use super::ThreadBufferedEvent;
use super::ThreadEventStore;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;

/// Returns the authoritative in-process turn view used by prompt editing.
///
/// Reusing this view avoids reparsing an entire non-paginated rollout merely to rediscover a
/// selected turn.
pub(super) fn materialized_thread_turns(store: &ThreadEventStore) -> Option<Vec<Turn>> {
    if !store.turn_history_complete {
        return None;
    }

    let mut turns = store.turns.clone();
    for event in &store.buffer {
        let ThreadBufferedEvent::Notification(notification) = event else {
            continue;
        };
        match notification.as_ref() {
            ServerNotification::TurnStarted(notification)
                if !turns.iter().any(|turn| turn.id == notification.turn.id) =>
            {
                turns.push(notification.turn.clone());
            }
            ServerNotification::ItemCompleted(notification) => {
                if matches!(
                    &notification.item,
                    ThreadItem::UserMessage { .. }
                        | ThreadItem::EnteredReviewMode { .. }
                        | ThreadItem::ExitedReviewMode { .. }
                ) && let Some(turn) = turns
                    .iter_mut()
                    .find(|turn| turn.id == notification.turn_id)
                    && !turn
                        .items
                        .iter()
                        .any(|item| item.id() == notification.item.id())
                {
                    turn.items.push(notification.item.clone());
                }
            }
            ServerNotification::TurnCompleted(notification) => {
                if let Some(turn) = turns
                    .iter_mut()
                    .find(|turn| turn.id == notification.turn.id)
                {
                    turn.status = notification.turn.status.clone();
                    turn.error = notification.turn.error.clone();
                    turn.started_at = notification.turn.started_at;
                    turn.completed_at = notification.turn.completed_at;
                    turn.duration_ms = notification.turn.duration_ms;
                }
            }
            _ => {}
        }
    }
    Some(turns)
}

pub(super) fn event_changes_materialized_turns(event: &ThreadBufferedEvent) -> bool {
    let ThreadBufferedEvent::Notification(notification) = event else {
        return false;
    };
    match notification.as_ref() {
        ServerNotification::TurnStarted(_) | ServerNotification::TurnCompleted(_) => true,
        ServerNotification::ItemCompleted(notification) => matches!(
            &notification.item,
            ThreadItem::UserMessage { .. }
                | ThreadItem::EnteredReviewMode { .. }
                | ThreadItem::ExitedReviewMode { .. }
        ),
        _ => false,
    }
}

#[cfg(test)]
#[path = "thread_turn_materialization_tests.rs"]
mod tests;
