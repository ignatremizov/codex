use super::common::ServerNotification;
use super::event_mapping::item_event_to_server_notification;
use super::thread_history::build_turns_from_rollout_items;
use super::v2::ItemCompletedNotification;
use super::v2::ItemStartedNotification;
use super::v2::MailboxReadItem;
use super::v2::MailboxReadSelector;
use super::v2::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::MailboxReadItem as CoreMailboxReadItem;
use codex_protocol::items::MailboxReadSelector as CoreMailboxReadSelector;
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
        input_batch: None,
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
fn array_send_outcomes_survive_transport_and_history_without_changing_old_items() {
    let mut call = send_item();
    call.input_batch = Some(codex_protocol::CollabAgentInputBatch {
        flags: "zf".into(),
        results: vec![
            codex_protocol::CollabAgentInputResult {
                target: "43".into(),
                receiver_thread_id: Some(call.receiver_thread_ids[0].to_string()),
                status: codex_protocol::CollabAgentInputStatus::MailboxAccepted,
                error: None,
                hint: None,
            },
            codex_protocol::CollabAgentInputResult {
                target: "unknown".into(),
                receiver_thread_id: None,
                status: codex_protocol::CollabAgentInputStatus::Error,
                error: Some("Unknown selector.".into()),
                hint: None,
            },
        ],
    });
    call.status = CollabAgentToolCallStatus::Failed;
    let item = TurnItem::CollabAgentToolCall(call.clone());
    let expected = ThreadItem::from(item.clone());
    let serialized = serde_json::to_value(&expected).unwrap();
    assert_eq!(
        serialized["inputBatch"],
        serde_json::json!({
            "flags": "zf",
            "results": [
                {"target":"43","receiverThreadId":call.receiver_thread_ids[0].to_string(),"status":"mailboxAccepted","error":null,"hint":null},
                {"target":"unknown","receiverThreadId":null,"status":"error","error":"Unknown selector.","hint":null}
            ]
        }),
    );
    assert_eq!(
        serde_json::from_value::<ThreadItem>(serialized).unwrap(),
        expected
    );
    assert_eq!(
        serde_json::from_value::<TurnItem>(serde_json::to_value(item).unwrap()).unwrap(),
        TurnItem::CollabAgentToolCall(call),
    );
    let old = send_item();
    let mut old_wire =
        serde_json::to_value(ThreadItem::from(TurnItem::CollabAgentToolCall(old.clone()))).unwrap();
    old_wire.as_object_mut().unwrap().remove("inputBatch");
    assert_eq!(
        serde_json::from_value::<ThreadItem>(old_wire).unwrap(),
        ThreadItem::from(TurnItem::CollabAgentToolCall(old)),
    );
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

#[test]
fn mailbox_read_item_survives_live_transport_and_rollout_replay() {
    let thread_id = ThreadId::new();
    let item = TurnItem::MailboxRead(CoreMailboxReadItem {
        id: "check-mail-1".to_string(),
        selector: CoreMailboxReadSelector::Agent { thread_id },
        consumed_count: 2,
        rejected_count: 1,
    });
    let expected = ThreadItem::MailboxRead(MailboxReadItem {
        id: "check-mail-1".to_string(),
        selector: MailboxReadSelector::Agent {
            thread_id: thread_id.to_string(),
        },
        consumed_count: 2,
        rejected_count: 1,
    });
    assert_eq!(
        serde_json::to_value(&expected).unwrap(),
        serde_json::json!({
            "type": "mailboxRead",
            "id": "check-mail-1",
            "selector": {
                "type": "agent",
                "threadId": thread_id.to_string()
            },
            "consumedCount": 2,
            "rejectedCount": 1
        }),
    );
    let completed = ItemCompletedEvent {
        thread_id,
        turn_id: "turn".to_string(),
        item,
        started_at_ms: None,
        completed_at_ms: 2,
    };

    let notification = item_event_to_server_notification(
        EventMsg::ItemCompleted(completed.clone()),
        &thread_id.to_string(),
        "turn",
    );
    let notification: ServerNotification =
        serde_json::from_value(serde_json::to_value(notification).unwrap()).unwrap();
    let ServerNotification::ItemCompleted(actual) = notification else {
        panic!("expected item/completed");
    };
    assert_eq!(
        actual,
        ItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn".to_string(),
            item: expected.clone(),
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
        RolloutItem::EventMsg(EventMsg::ItemCompleted(completed.clone())),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)),
    ];
    let replay: Vec<RolloutItem> =
        serde_json::from_str(&serde_json::to_string(&rollout).unwrap()).unwrap();
    let turns = build_turns_from_rollout_items(&replay);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items, vec![expected]);
}
