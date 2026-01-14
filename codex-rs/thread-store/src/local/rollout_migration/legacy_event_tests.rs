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
