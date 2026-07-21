use crate::bespoke_event_handling::now_unix_timestamp_ms;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadHistoryItemChange;
use codex_protocol::ThreadId;

pub(super) async fn emit_response_item_transcript_changes(
    conversation_id: ThreadId,
    changes: Vec<ThreadHistoryItemChange>,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let now = now_unix_timestamp_ms();
    for change in changes {
        let started_at_ms = change.started_at_ms.unwrap_or(now);
        let completed_at_ms = change.completed_at_ms.unwrap_or(started_at_ms);
        outgoing
            .send_server_notification(ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: change.turn_id.clone(),
                item: change.item.clone(),
                started_at_ms,
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
