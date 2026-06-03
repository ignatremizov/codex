use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentActivityEvent;
use codex_protocol::protocol::SubAgentActivityKind;
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
fn agent_message_does_not_invent_background_completion_provenance() {
    assert_converts_to(
        EventMsg::AgentMessage(AgentMessageEvent {
            message: "ordinary assistant output".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        }),
        (
            TurnItem::AgentMessage(AgentMessageItem {
                id: "item-1".to_string(),
                content: vec![AgentMessageContent::Text {
                    text: "ordinary assistant output".to_string(),
                }],
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
                sub_agent_completion: None,
            }),
            None,
        ),
    );
}

#[test]
fn context_compaction_preserves_visible_summary_and_message() {
    assert_converts_to(
        EventMsg::ContextCompacted(ContextCompactedEvent {
            summary: Some("compact summary".to_string()),
            message: Some("complete compacted prompt".to_string()),
            decode_error: Some("decoder unavailable".to_string()),
            available_skills: vec!["test-tui".to_string()],
        }),
        (
            TurnItem::ContextCompaction(ContextCompactionItem {
                id: "item-1".to_string(),
                summary: Some("compact summary".to_string()),
                message: Some("complete compacted prompt".to_string()),
                decode_error: Some("decoder unavailable".to_string()),
                available_skills: vec!["test-tui".to_string()],
            }),
            None,
        ),
    );
}

#[test]
fn subagent_activity_preserves_auditable_prompt() {
    let agent_thread_id = ThreadId::new();
    let agent_path = AgentPath::try_from("/root/reviewer").expect("valid agent path");
    assert_converts_to(
        EventMsg::SubAgentActivity(SubAgentActivityEvent {
            event_id: "activity-1".to_string(),
            occurred_at_ms: 1,
            agent_thread_id,
            agent_path: agent_path.clone(),
            kind: SubAgentActivityKind::Started,
            prompt: Some("Review the pagination migration.".to_string()),
        }),
        (
            TurnItem::SubAgentActivity(SubAgentActivityItem {
                id: "activity-1".to_string(),
                kind: SubAgentActivityKind::Started,
                agent_thread_id,
                agent_path,
                prompt: Some("Review the pagination migration.".to_string()),
            }),
            None,
        ),
    );
}
