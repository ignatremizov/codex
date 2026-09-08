use crate::ResponseItemEnvelope;
use crate::RolloutItem;
use crate::policy::persisted_rollout_items;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mailbox_delivery_response_item_id;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

fn identity(thread_id: ThreadId) -> AgentInputIdentity {
    AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    }
}

#[test]
fn mailbox_original_input_and_prepared_model_envelope_survive_both_history_modes() {
    let receiver = ThreadId::new();
    let id = mailbox_delivery_response_item_id(&ThreadId::new().to_string()).unwrap();
    let input = vec![UserInput::Image {
        image_url: "data:image/png;base64,b3JpZ2luYWw=".to_string(),
        detail: None,
    }];
    let user = TurnItem::UserMessage(UserMessageItem {
        id: id.to_string(),
        client_id: Some("client".to_string()),
        content: input.clone(),
    });
    let agent = TurnItem::AgentMessage(AgentMessageItem {
        id: id.to_string(),
        attribution: Some(AgentInputAttribution {
            sender: identity(ThreadId::new()),
            recipient: identity(receiver),
            sender_turn_id: "sender-turn".to_string(),
        }),
        input: Some(input),
        content: Vec::new(),
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
        sub_agent_completion: None,
    });
    for item in [user, agent] {
        let artifacts = vec![
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
                id: Some(id.clone()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "prepared media context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: receiver,
                turn_id: "check-mail-turn".to_string(),
                item,
                started_at_ms: None,
                completed_at_ms: 1,
            })),
        ];
        for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            assert_eq!(
                serde_json::to_value(persisted_rollout_items(&artifacts, mode)).unwrap(),
                serde_json::to_value(&artifacts).unwrap()
            );
        }

        let RolloutItem::EventMsg(EventMsg::ItemCompleted(mut completion)) = artifacts[1].clone()
        else {
            panic!("expected typed completion");
        };
        match &mut completion.item {
            TurnItem::UserMessage(item) => item.id = ResponseItemId::new("msg").to_string(),
            TurnItem::AgentMessage(item) => {
                let original = item.clone();
                item.input = None;
                let missing_input =
                    RolloutItem::EventMsg(EventMsg::ItemCompleted(completion.clone()));
                assert!(
                    persisted_rollout_items(&[missing_input], ThreadHistoryMode::Legacy).is_empty()
                );
                let TurnItem::AgentMessage(item) = &mut completion.item else {
                    panic!("expected agent input");
                };
                *item = original;
                item.attribution = None;
            }
            _ => panic!("expected mailbox input"),
        }
        assert!(
            persisted_rollout_items(
                &[RolloutItem::EventMsg(EventMsg::ItemCompleted(completion))],
                ThreadHistoryMode::Legacy,
            )
            .is_empty()
        );
    }
}
