use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;

use super::completed_item;

fn converted(event: EventMsg) -> (TurnItem, Option<String>) {
    completed_item(&event, &mut || Ok("item-1".to_string()))
        .expect("convert legacy event")
        .expect("legacy event should produce a completed item")
}

fn assert_converts_to(event: EventMsg, expected: (TurnItem, Option<String>)) {
    assert_eq!(
        serde_json::to_value(converted(event)).expect("serialize converted item"),
        serde_json::to_value(expected).expect("serialize expected item"),
    );
}

#[test]
fn context_compaction_preserves_visible_summary_and_message() {
    assert_converts_to(
        EventMsg::ContextCompacted(ContextCompactedEvent {
            summary: Some("compact summary".to_string()),
            message: Some("complete compacted prompt".to_string()),
        }),
        (
            TurnItem::ContextCompaction(ContextCompactionItem {
                id: "item-1".to_string(),
                summary: Some("compact summary".to_string()),
                message: Some("complete compacted prompt".to_string()),
            }),
            None,
        ),
    );
}

#[test]
fn historical_compaction_without_presentation_fields_remains_readable() {
    let event = serde_json::from_value::<EventMsg>(serde_json::json!({
        "type": "context_compacted"
    }))
    .expect("historical event");
    assert_converts_to(
        event,
        (
            TurnItem::ContextCompaction(ContextCompactionItem {
                id: "item-1".to_string(),
                summary: None,
                message: None,
            }),
            None,
        ),
    );
    let item = serde_json::from_value::<ContextCompactionItem>(serde_json::json!({
        "id": "old-item"
    }))
    .expect("historical canonical item");
    assert_eq!(
        serde_json::to_value(item).expect("serialize canonical item"),
        serde_json::json!({"id": "old-item"})
    );
}
