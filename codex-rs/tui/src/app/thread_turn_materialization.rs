//! Cached prompt selection is safe only after a complete canonical Legacy read.

use super::*;

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
            ServerNotification::TurnStarted(started) => {
                match turns.iter().find(|turn| turn.id == started.turn.id) {
                    Some(existing) if existing.status != TurnStatus::InProgress => return None,
                    Some(_) => {}
                    None => turns.push(started.turn.clone()),
                }
            }
            ServerNotification::ItemCompleted(completed) => {
                let mut matches = turns.iter_mut().filter(|turn| turn.id == completed.turn_id);
                let turn = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                if let Some(item) = turn
                    .items
                    .iter_mut()
                    .find(|item| item.id() == completed.item.id())
                {
                    *item = completed.item.clone();
                } else {
                    turn.items.push(completed.item.clone());
                }
            }
            ServerNotification::TurnCompleted(completed) => {
                let mut matches = turns.iter_mut().filter(|turn| turn.id == completed.turn.id);
                let turn = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                // Terminal notifications can omit items. Keep completed item receipts.
                if !completed.turn.items.is_empty() {
                    turn.items = completed.turn.items.clone();
                }
                turn.status = completed.turn.status.clone();
                turn.error = completed.turn.error.clone();
                turn.started_at = completed.turn.started_at;
                turn.completed_at = completed.turn.completed_at;
                turn.duration_ms = completed.turn.duration_ms;
            }
            _ => {}
        }
    }
    Some(turns)
}

pub(super) fn event_changes_materialized_turns(event: &ThreadBufferedEvent) -> bool {
    matches!(event, ThreadBufferedEvent::Notification(notification)
        if matches!(notification.as_ref(),
            ServerNotification::TurnStarted(_)
                | ServerNotification::TurnCompleted(_)
                | ServerNotification::ItemStarted(_)
                | ServerNotification::ItemCompleted(_)
                | ServerNotification::ThreadReverted(_)))
}

#[cfg(test)]
#[path = "thread_turn_materialization_tests.rs"]
mod tests;
