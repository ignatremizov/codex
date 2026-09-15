use super::*;
use crate::ThreadId;
use crate::items::CollabAgentToolCallItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn old_single_send_keeps_legacy_projection_but_array_send_does_not_select_first_receiver() {
    let sender = ThreadId::new();
    let receiver = ThreadId::new();
    let mut call: CollabAgentToolCallItem = serde_json::from_value(json!({
        "id": "send",
        "tool": "send_input",
        "status": "completed",
        "sender_thread_id": sender,
        "receiver_thread_ids": [receiver],
    }))
    .unwrap();
    assert_eq!(call.input_batch, None);
    assert!(call.as_legacy_begin_event(/*started_at_ms*/ 1).is_some());
    assert!(call.as_legacy_end_event(/*completed_at_ms*/ 2).is_some());
    call.input_batch = Some(CollabAgentInputBatch {
        flags: "z".into(),
        results: Vec::new(),
    });
    assert!(call.as_legacy_begin_event(/*started_at_ms*/ 1).is_none());
    assert!(call.as_legacy_end_event(/*completed_at_ms*/ 2).is_none());
    assert_eq!(
        serde_json::from_value::<CollabAgentToolCallItem>(serde_json::to_value(&call).unwrap())
            .unwrap(),
        call,
    );
}
