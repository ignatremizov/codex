use super::*;
use crate::event_processor_with_jsonl_output::EventProcessorWithJsonOutput;
use crate::exec_events::ThreadEvent;
use crate::exec_events::ThreadItemDetails;
use pretty_assertions::assert_eq;
use serde_json::json;

fn inbound_item() -> ThreadItem {
    serde_json::from_value(json!({
        "type": "agentMessage", "id": "inbound", "text": "",
        "attribution": {
            "sender": {"threadId": "01900000-0000-7000-8000-000000000001"},
            "recipient": {"threadId": "01900000-0000-7000-8000-000000000002"},
            "senderTurnId": "sender-turn"
        },
        "input": [
            {"type": "text", "text": "  **exact inbound**\n</agent_message>  "},
            {"type": "image", "url": "data:image/png;base64,secret-image"}
        ]
    }))
    .unwrap()
}

fn own_reply() -> ThreadItem {
    serde_json::from_value(json!({
        "type": "agentMessage", "id": "own-reply", "text": "own final reply"
    }))
    .unwrap()
}

fn item_completed(item: ThreadItem) -> ServerNotification {
    ServerNotification::ItemCompleted(codex_app_server_protocol::ItemCompletedNotification {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        completed_at_ms: 1,
        item,
    })
}

fn turn_completed(items: Vec<ThreadItem>) -> ServerNotification {
    ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
        thread_id: "thread-1".into(),
        turn: codex_app_server_protocol::Turn {
            id: "turn-1".into(),
            items,
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    })
}

#[test]
fn human_inbound_input_never_replaces_own_final_or_output_file() {
    let home = tempfile::tempdir().unwrap();
    let output = home.path().join("last-message.txt");
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: false,
        show_raw_agent_reasoning: false,
        show_compact_summary: false,
        last_message_path: Some(output.clone()),
        final_message: None,
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };
    processor.process_server_notification(item_completed(inbound_item()));
    assert_eq!(
        (
            processor.final_message.clone(),
            processor.final_message_rendered
        ),
        (None, false)
    );
    processor.process_server_notification(item_completed(own_reply()));
    processor.process_server_notification(item_completed(inbound_item()));
    assert_eq!(
        (
            processor.final_message.clone(),
            processor.final_message_rendered
        ),
        (Some("own final reply".into()), true)
    );
    assert_eq!(final_message_from_turn_items(&[inbound_item()]), None);
    processor.process_server_notification(turn_completed(vec![own_reply(), inbound_item()]));
    processor.print_final_output();
    assert_eq!(std::fs::read_to_string(output).unwrap(), "own final reply");
}

#[test]
fn jsonl_exports_inbound_identity_and_exact_text_without_replacing_own_final() {
    let home = tempfile::tempdir().unwrap();
    let output = home.path().join("last-message.txt");
    let mut processor = EventProcessorWithJsonOutput::new(Some(output.clone()));
    let collected = processor.collect_thread_events(item_completed(inbound_item()));
    assert_eq!(processor.final_message(), None);
    let [ThreadEvent::ItemCompleted(event)] = collected.events.as_slice() else {
        panic!("expected one completed inbound item");
    };
    assert!(matches!(
        &event.item.details,
        ThreadItemDetails::AgentInput(_)
    ));
    assert_eq!(
        serde_json::to_value(&event.item.details).unwrap(),
        json!({
            "type": "agent_input",
            "sender_thread_id": "01900000-0000-7000-8000-000000000001",
            "recipient_thread_id": "01900000-0000-7000-8000-000000000002",
            "text": "Agent message from `01900000-0000-7000-8000-000000000001` to \
                     `01900000-0000-7000-8000-000000000002`:\n\n  **exact inbound**\n\
                     </agent_message>  \n[image]"
        })
    );
    assert_eq!(
        serde_json::from_value::<ThreadItemDetails>(
            serde_json::to_value(&event.item.details).unwrap()
        )
        .unwrap(),
        event.item.details
    );
    processor.collect_thread_events(item_completed(own_reply()));
    processor.collect_thread_events(item_completed(inbound_item()));
    assert_eq!(processor.final_message(), Some("own final reply"));
    processor.collect_thread_events(turn_completed(vec![own_reply(), inbound_item()]));
    processor.print_final_output();
    assert_eq!(std::fs::read_to_string(output).unwrap(), "own final reply");
}
