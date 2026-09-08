use super::common::ServerNotification;
use super::event_mapping::item_event_to_server_notification;
use super::thread_history::build_turns_from_rollout_items;
use super::v2::ItemCompletedNotification;
use super::v2::ItemStartedNotification;
use super::v2::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

fn send_item() -> CollabAgentToolCallItem {
    CollabAgentToolCallItem {
        id: "mail-send".to_string(),
        tool: CollabAgentTool::SendInput,
        status: CollabAgentToolCallStatus::InProgress,
        observe_commentary: Some(false),
        wake_on_completion: None,
        target_messages: Some(false),
        queue_input: Some(false),
        mailbox_input: Some(true),
        deadline_at_ms: None,
        sender_thread_id: ThreadId::new(),
        receiver_thread_ids: vec![ThreadId::new()],
        receiver_agents: Vec::new(),
        prompt: Some("Ordinary text without delivery hints.".to_string()),
        model: None,
        reasoning_effort: None,
        agents_states: Default::default(),
        completion_presentation_agent_ids: None,
    }
}

#[test]
fn structured_mailbox_metadata_survives_live_transport_and_rollout_replay() {
    for mailbox_input in [Some(true), Some(false), None] {
        for status in [
            CollabAgentToolCallStatus::Completed,
            CollabAgentToolCallStatus::Failed,
        ] {
            let mut call = send_item();
            call.mailbox_input = mailbox_input;
            let thread_id = call.sender_thread_id;
            let started_item = TurnItem::CollabAgentToolCall(call.clone());
            let started = ItemStartedEvent {
                thread_id,
                turn_id: "turn".to_string(),
                item: started_item.clone(),
                started_at_ms: 1,
            };
            let expected_started = ThreadItem::from(started_item);
            assert_eq!(
                serde_json::to_value(&expected_started).unwrap()["mailboxInput"],
                serde_json::to_value(mailbox_input).unwrap(),
            );
            let notification = item_event_to_server_notification(
                EventMsg::ItemStarted(started.clone()),
                &thread_id.to_string(),
                "turn",
            );
            let notification: ServerNotification =
                serde_json::from_value(serde_json::to_value(notification).unwrap()).unwrap();
            let ServerNotification::ItemStarted(actual_started) = notification else {
                panic!("expected item/started");
            };
            assert_eq!(
                actual_started,
                ItemStartedNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn".to_string(),
                    item: expected_started.clone(),
                    started_at_ms: 1,
                    deadline_at_ms: None,
                },
            );

            call.status = status;
            let completed_item = TurnItem::CollabAgentToolCall(call);
            let expected_completed = ThreadItem::from(completed_item.clone());
            let completed = ItemCompletedEvent {
                thread_id,
                turn_id: "turn".to_string(),
                item: completed_item,
                started_at_ms: Some(1),
                completed_at_ms: 2,
            };
            let notification = item_event_to_server_notification(
                EventMsg::ItemCompleted(completed.clone()),
                &thread_id.to_string(),
                "turn",
            );
            let notification: ServerNotification =
                serde_json::from_value(serde_json::to_value(notification).unwrap()).unwrap();
            let ServerNotification::ItemCompleted(actual_completed) = notification else {
                panic!("expected item/completed");
            };
            assert_eq!(
                actual_completed,
                ItemCompletedNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn".to_string(),
                    item: expected_completed.clone(),
                    completed_at_ms: 2,
                },
            );
            let rollout = vec![
                RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn".to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                })),
                RolloutItem::EventMsg(EventMsg::ItemStarted(started)),
                RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)),
            ];
            let replay: Vec<RolloutItem> =
                serde_json::from_str(&serde_json::to_string(&rollout).unwrap()).unwrap();
            let started_turns = build_turns_from_rollout_items(&replay[..2]);
            assert_eq!(started_turns.len(), 1);
            assert_eq!(started_turns[0].items, vec![expected_started]);
            let turns = build_turns_from_rollout_items(&replay);
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].items, vec![expected_completed]);
        }
    }
}

#[test]
fn legacy_omission_remains_unknown_without_inference() {
    let mut call = send_item();
    call.mailbox_input = None;
    call.prompt = Some("mailbox send_input w:z".to_string());
    let serialized = serde_json::to_value(&call).unwrap();
    assert!(serialized.get("mailbox_input").is_none());
    assert_eq!(
        serde_json::from_value::<CollabAgentToolCallItem>(serialized).unwrap(),
        call,
    );

    let expected = ThreadItem::from(TurnItem::CollabAgentToolCall(call.clone()));
    let mut old_wire = serde_json::to_value(&expected).unwrap();
    assert_eq!(
        old_wire.as_object_mut().unwrap().remove("mailboxInput"),
        Some(serde_json::Value::Null),
    );
    assert_eq!(
        serde_json::from_value::<ThreadItem>(old_wire).unwrap(),
        expected,
    );

    let started = ItemStartedEvent {
        thread_id: call.sender_thread_id,
        turn_id: "legacy-turn".to_string(),
        item: TurnItem::CollabAgentToolCall(call),
        started_at_ms: 1,
    };
    let legacy = started
        .as_legacy_events(/*show_raw_agent_reasoning*/ false)
        .pop()
        .unwrap();
    let notification = item_event_to_server_notification(
        legacy.clone(),
        &started.thread_id.to_string(),
        &started.turn_id,
    );
    let ServerNotification::ItemStarted(actual) = notification else {
        panic!("expected legacy item/started");
    };
    let mut expected_legacy = expected;
    let ThreadItem::CollabAgentToolCall {
        observe_commentary,
        target_messages,
        queue_input,
        ..
    } = &mut expected_legacy
    else {
        panic!("expected collaboration item");
    };
    *observe_commentary = None;
    *target_messages = None;
    *queue_input = None;
    assert_eq!(actual.item, expected_legacy);
    let turns = build_turns_from_rollout_items(&[RolloutItem::EventMsg(legacy)]);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items, vec![expected_legacy]);
}
