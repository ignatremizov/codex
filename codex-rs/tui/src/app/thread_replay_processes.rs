//! Only the currently running turn can retain process-local terminals during replay.

use super::*;

pub(super) fn active_snapshot_turn(snapshot: &ThreadEventSnapshot) -> Option<String> {
    let mut active = snapshot
        .turns
        .iter()
        .rev()
        .find(|turn| turn.status == TurnStatus::InProgress)
        .map(|turn| turn.id.clone());
    for event in &snapshot.events {
        let ThreadBufferedEvent::Notification(notification) = event else {
            continue;
        };
        match notification.as_ref() {
            ServerNotification::TurnStarted(started) => active = Some(started.turn.id.clone()),
            ServerNotification::TurnCompleted(completed)
                if active.as_ref() == Some(&completed.turn.id) =>
            {
                active = None
            }
            _ => {}
        }
    }
    active
}

pub(super) fn item_replay_kind(
    event: &ThreadBufferedEvent,
    active_turn: Option<&str>,
    kind: ReplayKind,
) -> ReplayKind {
    if kind != ReplayKind::ThreadSnapshot {
        return kind;
    }
    let turn = match event {
        ThreadBufferedEvent::Notification(notification) => match notification.as_ref() {
            ServerNotification::ItemStarted(item) => Some(item.turn_id.as_str()),
            ServerNotification::ItemCompleted(item) => Some(item.turn_id.as_str()),
            _ => None,
        },
        _ => None,
    };
    if active_turn.is_none() || turn.is_some_and(|turn| Some(turn) != active_turn) {
        ReplayKind::ReplayOnlyThreadSnapshot
    } else {
        kind
    }
}

#[cfg(test)]
#[path = "thread_replay_processes_tests.rs"]
mod tests;
