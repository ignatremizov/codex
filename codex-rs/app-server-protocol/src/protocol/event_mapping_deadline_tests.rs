use super::*;
use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::ModelInvocationContext;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TerminalInteractionEvent;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn modern_and_legacy_wait_starts_preserve_optional_deadlines() {
    let thread_id = ThreadId::new();
    for deadline_at_ms in [None, Some(1_750_000_005_000_i64)] {
        let items = [
            json!({
                "type": "CommandExecution", "id": "exec-1",
                "deadline_at_ms": deadline_at_ms,
                "plugin_id": "sample@openai-curated", "script_path": "scripts/run.py",
                "process_id": "1000", "command": ["echo", "done"], "cwd": "file:///tmp",
                "parsed_cmd": [], "source": "unified_exec_startup", "status": "in_progress",
            }),
            json!({
                "type": "CollabAgentToolCall", "id": "wait-1",
                "deadline_at_ms": deadline_at_ms,
                "tool": "wait", "status": "in_progress", "sender_thread_id": thread_id,
            }),
        ];
        for item in items {
            let item: TurnItem = serde_json::from_value(item).expect("valid started item");
            let started = ItemStartedEvent {
                thread_id,
                turn_id: "turn-1".to_string(),
                item,
                started_at_ms: 123,
            };
            let expected = ItemStartedNotification {
                thread_id: thread_id.to_string(),
                turn_id: started.turn_id.clone(),
                item: started.item.clone().into(),
                started_at_ms: started.started_at_ms,
                deadline_at_ms,
            };
            let mut events = started.as_legacy_events(/*show_raw_agent_reasoning*/ false);
            assert_eq!(events.len(), 1);
            events.push(EventMsg::ItemStarted(started));
            for event in events {
                let mapped =
                    item_event_to_server_notification(event, &thread_id.to_string(), "turn-1");
                let ServerNotification::ItemStarted(actual) = mapped else {
                    panic!("expected item started");
                };
                assert_eq!(actual, expected);
                let wire = serde_json::to_value(&actual).expect("serialize notification");
                assert_eq!(wire["deadlineAtMs"], json!(deadline_at_ms));
                assert!(
                    !wire["item"]
                        .as_object()
                        .expect("item")
                        .contains_key("deadlineAtMs")
                );
            }
        }
    }
}

#[test]
fn command_deadline_mapping_preserves_runtime_model_and_plugin_attribution() {
    let mut command: CommandExecutionItem = serde_json::from_value(json!({
        "id": "exec-1", "deadline_at_ms": 500,
        "plugin_id": "sample@openai-curated", "script_path": "scripts/run.py",
        "command": ["echo", "done"], "cwd": "file:///tmp", "parsed_cmd": [],
        "source": "unified_exec_startup", "status": "in_progress",
    }))
    .expect("command item");
    command.model_context = Some(ModelInvocationContext {
        model_slug: "model-test".to_string(),
        reasoning_effort: Some("high".to_string()),
    });
    let item = TurnItem::CommandExecution(command);
    let expected = ItemStartedNotification {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        item: item.clone().into(),
        started_at_ms: 123,
        deadline_at_ms: Some(500),
    };
    let mapped = item_event_to_server_notification(
        EventMsg::ItemStarted(ItemStartedEvent {
            thread_id: ThreadId::new(),
            turn_id: "turn-1".to_string(),
            item,
            started_at_ms: 123,
        }),
        "thread-1",
        "turn-1",
    );
    let ServerNotification::ItemStarted(actual) = mapped else {
        panic!("expected item started");
    };
    assert_eq!(actual, expected);
}

#[test]
fn terminal_poll_begin_and_clear_keep_original_exec_identity_and_nullable_wire_field() {
    for deadline_at_ms in [Some(1_750_000_005_000_i64), None] {
        let mapped = item_event_to_server_notification(
            EventMsg::TerminalInteraction(TerminalInteractionEvent {
                call_id: "original-exec".to_string(),
                process_id: "1000".to_string(),
                stdin: String::new(),
                deadline_at_ms,
                wait: None,
            }),
            "thread-1",
            "turn-2",
        );
        let ServerNotification::TerminalInteraction(actual) = mapped else {
            panic!("expected terminal interaction");
        };
        assert_eq!(
            serde_json::to_value(actual).expect("serialize notification"),
            json!({
                "threadId": "thread-1", "turnId": "turn-2",
                "itemId": "original-exec", "processId": "1000",
                "stdin": "", "deadlineAtMs": deadline_at_ms,
            })
        );
    }
}
