//! Non-owning audit receipts must not replace the active model turn during replay.

use super::ThreadHistoryBuilder;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
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
        let thread_id = ThreadId::new();
        let activity_id = item.id();
        let expected = ThreadItem::from(item.clone());
        let mut builder = ThreadHistoryBuilder::new();
        builder.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "active-model-turn".to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: Some(100),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        }));
        let root_before = builder
            .active_turn_snapshot()
            .expect("model turn is active");

        for event in [
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id,
                turn_id: activity_id.clone(),
                item: item.clone(),
                started_at_ms: 100,
            }),
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: activity_id.clone(),
                item,
                started_at_ms: Some(100),
                completed_at_ms: 123,
            }),
        ] {
            builder.handle_event(&event);
            assert_eq!(builder.active_turn_snapshot(), Some(root_before.clone()));
            let activity = builder
                .turn_snapshot(&activity_id)
                .expect("audit turn exists");
            assert_eq!(
                (activity.id, activity.status, activity.items),
                (
                    activity_id.clone(),
                    TurnStatus::Completed,
                    vec![expected.clone()],
                )
            );
        }
        assert_eq!(builder.finish().len(), 2);
    }
}
