//! Transport receipts and Cloud stream loss keep their distinct thread scopes.

use super::event_targets_thread;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn rollback_fence_classifies_cloud_stream_loss_like_its_thread_notification() {
    let affected = ThreadId::new();
    let other = ThreadId::new();
    let error: ErrorNotification = serde_json::from_value(serde_json::json!({
        "threadId": affected.to_string(), "turnId": "", "willRetry": false,
        "error": { "message": "Cloud stream ended" },
    }))
    .expect("Cloud stream notification");
    for event in [
        AppServerEvent::ThreadStreamDisconnected(Box::new(error.clone())),
        AppServerEvent::ServerNotification(Box::new(ServerNotification::Error(error))),
    ] {
        assert_eq!(
            (
                event_targets_thread(&event, affected),
                event_targets_thread(&event, other)
            ),
            (true, false),
        );
    }
    let boundary = AppServerEvent::RequestCompleted {
        request_id: RequestId::String(affected.to_string()),
    };
    assert!(!event_targets_thread(&boundary, affected));
}
