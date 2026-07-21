use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadHistoryItemChange;
use codex_protocol::ThreadId;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(super) async fn emit_response_item_transcript_changes(
    conversation_id: ThreadId,
    changes: Vec<ThreadHistoryItemChange>,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let completed_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    for change in changes {
        outgoing
            .send_server_notification(ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: change.turn_id.clone(),
                item: change.item.clone(),
                started_at_ms: completed_at_ms,
                deadline_at_ms: None,
            }))
            .await;
        outgoing
            .send_server_notification(ServerNotification::ItemCompleted(
                ItemCompletedNotification {
                    thread_id: conversation_id.to_string(),
                    turn_id: change.turn_id,
                    item: change.item,
                    completed_at_ms,
                },
            ))
            .await;
    }
}
