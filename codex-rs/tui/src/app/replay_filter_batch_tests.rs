use super::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn batch_refresh_retention_requires_matching_foreign_source_provenance() {
    let root = ThreadId::new();
    let sender = ThreadId::new();
    let source_id = format!(
        "agent-input-batch/{}",
        serde_json::to_string(&(sender, "source/turn", "opaque/call")).unwrap()
    );
    let valid = json!({
        "method": "item/completed",
        "params": {
            "threadId": root, "turnId": "root-turn", "completedAtMs": 1,
            "item": {
                "type": "collabAgentToolCall", "id": source_id,
                "tool": "sendInput", "status": "failed",
                "senderThreadId": sender, "receiverThreadIds": [], "receiverAgents": [],
                "agentsStates": {}, "prompt": "shared payload",
                "inputBatch": {"flags": "z", "senderThreadId": sender, "results": []}
            }
        }
    });
    let notification: ServerNotification = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(
        mirrored_completion_item_id(&notification),
        Some(source_id.as_str())
    );

    for (pointer, replacement) in [
        ("/params/threadId", json!(sender)),
        ("/params/item/senderThreadId", json!(ThreadId::new())),
        (
            "/params/item/inputBatch/senderThreadId",
            json!(ThreadId::new()),
        ),
        ("/params/item/inputBatch/senderThreadId", json!(null)),
        (
            "/params/item/id",
            json!("agent-input-batch/not-a-source-tuple"),
        ),
        (
            "/params/item/id",
            json!(format!(
                "agent-input-batch/{}",
                serde_json::to_string(&(sender, "", "opaque/call")).unwrap()
            )),
        ),
        (
            "/params/item/id",
            json!(format!(
                "agent-input-batch/{}",
                serde_json::to_string(&(sender, "source/turn", "")).unwrap()
            )),
        ),
        ("/params/item/tool", json!("wait")),
        ("/params/item/status", json!("inProgress")),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let notification: ServerNotification = serde_json::from_value(changed).unwrap();
        assert_eq!(
            mirrored_completion_item_id(&notification),
            None,
            "{pointer}"
        );
    }
}
