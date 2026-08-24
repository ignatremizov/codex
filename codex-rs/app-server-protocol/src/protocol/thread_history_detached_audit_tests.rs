//! Non-owning audit receipts must not replace the active model turn during replay.

use super::ThreadHistoryBuilder;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::ThreadItemPayload;
use crate::protocol::v2::TurnStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use pretty_assertions::assert_eq;

#[test]
fn standalone_agent_audits_do_not_replace_the_active_model_turn() {
    let mut peer_message = AgentMessageItem::new(&[AgentMessageContent::Text {
        text: "A peer supplied additional context.".to_string(),
    }]);
    peer_message.id = new_attributed_agent_message_response_item_id().to_string();
    peer_message.phase = Some(MessagePhase::Commentary);
    let completion = sub_agent_completion_item(
        "/root/worker",
        &AgentStatus::Completed(Some("Work completed.".to_string())),
    )
    .expect("completed status creates a canonical receipt");

    for item in [
        TurnItem::AgentMessage(peer_message),
        TurnItem::AgentMessage(completion),
        TurnItem::UserAgentControl(UserAgentControlItem::succeeded(
            UserAgentControlAction::Prompt,
        )),
    ] {
        for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            for include_start in [false, true] {
                let thread_id = ThreadId::new();
                let activity_id = item.id();
                let mut builder = ThreadHistoryBuilder::new();
                builder.handle_event_for_history_mode(
                    &EventMsg::TurnStarted(TurnStartedEvent {
                        turn_attribution: None,
                        turn_id: "active-model-turn".to_string(),
                        root_turn_id: None,
                        trace_id: None,
                        started_at: Some(100),
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                        agent_queue: None,
                    }),
                    mode,
                );
                let compaction = ContextCompactionItem {
                    id: "active-compaction".to_string(),
                    summary: Some("Retained summary".to_string()),
                    message: Some("Retained complete compacted prompt".to_string()),
                    decode_error: None,
                    available_skills: vec!["retained-skill".to_string()],
                };
                builder.handle_event_for_history_mode(
                    &EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id,
                        turn_id: "active-model-turn".to_string(),
                        item: TurnItem::ContextCompaction(compaction.clone()),
                        started_at_ms: Some(10),
                        completed_at_ms: 20,
                    }),
                    mode,
                );
                let root_before = builder
                    .active_turn_snapshot()
                    .expect("model turn is active");
                let mut events = Vec::new();
                if include_start {
                    events.push(EventMsg::ItemStarted(ItemStartedEvent {
                        thread_id,
                        turn_id: activity_id.clone(),
                        item: item.clone(),
                        started_at_ms: 100,
                    }));
                }
                events.push(EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id,
                    turn_id: activity_id.clone(),
                    item: item.clone(),
                    started_at_ms: Some(100),
                    completed_at_ms: 123,
                }));

                for event in events {
                    let completed_at_ms = match &event {
                        EventMsg::ItemStarted(_) => None,
                        EventMsg::ItemCompleted(_) => Some(123),
                        _ => unreachable!("fixture contains only item lifecycle events"),
                    };
                    let expected = ThreadItem {
                        payload: ThreadItemPayload::from(item.clone()),
                        started_at_ms: Some(100),
                        completed_at_ms,
                    };
                    builder.handle_event_for_history_mode(&event, mode);
                    assert_eq!(builder.active_turn_snapshot(), Some(root_before.clone()));
                    let activity = builder
                        .turn_snapshot(&activity_id)
                        .expect("audit turn exists");
                    assert_eq!(
                        (activity.id, activity.status, activity.items),
                        (activity_id.clone(), TurnStatus::Completed, vec![expected],)
                    );
                }
                // Detached activity cannot discard the pending canonical/legacy compaction pair.
                builder.handle_event_for_history_mode(
                    &EventMsg::ContextCompacted(ContextCompactedEvent {
                        summary: compaction.summary,
                        message: compaction.message,
                        decode_error: compaction.decode_error,
                        available_skills: compaction.available_skills,
                    }),
                    mode,
                );
                assert_eq!(builder.active_turn_snapshot(), Some(root_before));
                assert_eq!(builder.finish().len(), 2);
            }
        }
    }
}
