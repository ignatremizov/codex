use crate::items::CollabAgentToolCallItem;
use crate::items::CommandExecutionItem;
use crate::protocol::CollabWaitingBeginEvent;
use crate::protocol::ExecCommandBeginEvent;
use crate::protocol::TerminalInteractionEvent;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;

fn assert_backward_compatible_deadline<T: DeserializeOwned + Serialize>(old: Value) {
    let decoded: T = serde_json::from_value(old.clone()).expect("old event should decode");
    let canonical = serde_json::to_value(decoded).expect("serialize old event");
    assert!(
        !canonical
            .as_object()
            .expect("event object")
            .contains_key("deadline_at_ms")
    );

    for deadline in [Value::Null, json!(1_750_000_005_000_i64)] {
        let mut updated = old.clone();
        updated["deadline_at_ms"] = deadline.clone();
        let decoded: T = serde_json::from_value(updated).expect("deadline event should decode");
        let mut expected = canonical.clone();
        if !deadline.is_null() {
            expected["deadline_at_ms"] = deadline;
        }
        assert_eq!(
            serde_json::to_value(decoded).expect("serialize event"),
            expected
        );
    }
}

#[test]
fn historical_command_and_collaboration_events_decode_without_deadlines() {
    assert_backward_compatible_deadline::<CommandExecutionItem>(json!({
        "id": "exec-1",
        "command": ["echo", "done"],
        "cwd": "file:///tmp",
        "parsed_cmd": [],
        "source": "unified_exec_startup",
        "status": "in_progress",
    }));
    assert_backward_compatible_deadline::<ExecCommandBeginEvent>(json!({
        "call_id": "exec-1",
        "turn_id": "turn-1",
        "command": ["echo", "done"],
        "cwd": "file:///tmp",
        "parsed_cmd": [],
    }));
    assert_backward_compatible_deadline::<TerminalInteractionEvent>(json!({
        "call_id": "exec-1", "process_id": "1000", "stdin": "",
    }));
    let sender = "00000000-0000-4000-8000-000000000001";
    assert_backward_compatible_deadline::<CollabAgentToolCallItem>(json!({
        "id": "wait-1", "tool": "wait", "status": "in_progress",
        "sender_thread_id": sender,
    }));
    assert_backward_compatible_deadline::<CollabWaitingBeginEvent>(json!({
        "call_id": "wait-1", "sender_thread_id": sender, "receiver_thread_ids": [],
    }));
}
