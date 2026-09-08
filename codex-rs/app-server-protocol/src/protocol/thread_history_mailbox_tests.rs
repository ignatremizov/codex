//! Canonical mailbox completions are the payload-bearing Legacy transcript records.

use super::ThreadHistoryBuilder;
use super::build_turns_from_rollout_items;
use crate::protocol::v2::ThreadItem;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mailbox_delivery_response_item_id;
use codex_protocol::models::ImageReference;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

fn start(turn_id: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
        agent_queue: None,
    })
}

fn finish(turn_id: &str) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    })
}

fn deliveries(receiver: ThreadId, turn_id: &str) -> Vec<ItemCompletedEvent> {
    let input = vec![
        UserInput::Text {
            text: "Original mailbox text".into(),
            text_elements: Vec::new(),
        },
        UserInput::Image {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,original-mailbox-image".into(),
            },
            detail: None,
        },
        UserInput::Mention {
            name: "original resource".into(),
            path: "app://mailbox/resource".into(),
        },
    ];
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: Some("Original name".into()),
        agent_ref: Some("7".into()),
        task_path: Some("/root/original-task".into()),
        role: Some("reviewer".into()),
        model: Some("original-model".into()),
        reasoning_effort: None,
    };
    let id = || {
        mailbox_delivery_response_item_id(&uuid::Uuid::now_v7().to_string())
            .expect("canonical mailbox delivery identity")
            .to_string()
    };
    let mut agent = AgentMessageItem::new(&[]);
    agent.id = id();
    agent.phase = Some(MessagePhase::Commentary);
    agent.attribution = Some(AgentInputAttribution {
        sender: identity(ThreadId::new()),
        recipient: identity(receiver),
        sender_turn_id: "sender-turn".into(),
    });
    agent.input = Some(input.clone());
    vec![
        TurnItem::UserMessage(UserMessageItem {
            id: id(),
            client_id: Some("original-client-id".into()),
            content: input,
        }),
        TurnItem::AgentMessage(agent),
    ]
    .into_iter()
    .map(|item| ItemCompletedEvent {
        thread_id: receiver,
        turn_id: turn_id.into(),
        item,
        started_at_ms: None,
        completed_at_ms: 123,
    })
    .collect()
}

#[test]
fn mailbox_completions_keep_full_payloads_and_do_not_duplicate_late_replay() {
    let completions = deliveries(ThreadId::new(), "consuming-turn");
    let expected = completions
        .iter()
        .map(|event| ThreadItem::from(event.item.clone()))
        .collect::<Vec<_>>();
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_event(&start("consuming-turn"));
    for event in &completions {
        // Started events are not the persisted mailbox delivery contract.
        builder.handle_event(&EventMsg::ItemStarted(ItemStartedEvent {
            thread_id: event.thread_id,
            turn_id: event.turn_id.clone(),
            item: event.item.clone(),
            started_at_ms: 100,
        }));
    }
    assert!(
        builder
            .active_turn_snapshot()
            .expect("active turn")
            .items
            .is_empty()
    );
    for event in &completions {
        builder.handle_event(&EventMsg::ItemCompleted(event.clone()));
    }
    builder.handle_event(&finish("consuming-turn"));
    let completed = builder
        .turn_snapshot("consuming-turn")
        .expect("consuming turn");
    assert_eq!(completed.items, expected);
    builder.handle_event(&start("new-turn"));
    let active = builder.active_turn_snapshot();
    for event in completions {
        builder.handle_event(&EventMsg::ItemCompleted(event));
    }
    assert_eq!(builder.active_turn_snapshot(), active);
    assert_eq!(builder.turn_snapshot("consuming-turn"), Some(completed));
}

#[test]
fn ordinary_and_invalid_mailbox_completions_do_not_gain_authoritative_payload_rows() {
    let mut invalid = deliveries(ThreadId::new(), "turn");
    let TurnItem::UserMessage(user) = &mut invalid[0].item else {
        unreachable!();
    };
    user.id = "ordinary-user-item".into();
    let mut foreign = invalid[1].clone();
    foreign.thread_id = ThreadId::new();
    invalid.push(foreign);
    let TurnItem::AgentMessage(agent) = &mut invalid[1].item else {
        unreachable!();
    };
    agent.input = None;
    let mut malformed = deliveries(ThreadId::new(), "turn").remove(1);
    let TurnItem::AgentMessage(agent) = &mut malformed.item else {
        unreachable!();
    };
    agent.id = "msg_mailbox_invalid".into();
    invalid.push(malformed);
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_event(&start("turn"));
    let before = builder.active_turn_snapshot();
    for event in invalid {
        builder.handle_event(&EventMsg::ItemCompleted(event));
    }
    assert_eq!(builder.active_turn_snapshot(), before);
}

#[test]
fn exact_rollback_filters_mailbox_payloads_with_their_consuming_turn() {
    let receiver = ThreadId::new();
    let mut history = Vec::new();
    let mut cut = 0;
    for turn_id in ["retained", "removed"] {
        if turn_id == "removed" {
            cut = history.len();
        }
        history.push(RolloutItem::EventMsg(start(turn_id)));
        history.extend(
            deliveries(receiver, turn_id)
                .into_iter()
                .map(|event| RolloutItem::EventMsg(EventMsg::ItemCompleted(event))),
        );
        history.push(RolloutItem::EventMsg(finish(turn_id)));
    }
    let before = build_turns_from_rollout_items(&history[..cut]);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].items.len(), 2);
    history.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: Some(cut as u64),
        },
    )));
    assert_eq!(build_turns_from_rollout_items(&history), before);
}
