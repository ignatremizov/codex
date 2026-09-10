use super::common::ServerNotification;
use super::event_mapping::item_event_to_server_notification;
use super::thread_history::build_turns_from_rollout_items;
use super::v2::CollabAgentRef;
use super::v2::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CollabAgentRef as CoreCollabAgentRef;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn spawned_identity_survives_live_wire_and_replay() {
    let sender = ThreadId::new();
    let receiver = ThreadId::new();
    let agent = CoreCollabAgentRef {
        thread_id: receiver,
        agent_ref: Some("2".to_string()),
        task_path: Some("/root/backend/auth".to_string()),
        agent_nickname: Some("Pascal".to_string()),
        agent_role: Some("coder".to_string()),
    };
    assert_eq!(
        CollabAgentRef::from(agent.clone()),
        CollabAgentRef {
            thread_id: receiver.to_string(),
            agent_ref: Some("2".to_string()),
            task_path: Some("/root/backend/auth".to_string()),
            agent_nickname: Some("Pascal".to_string()),
            agent_role: Some("coder".to_string()),
        },
    );
    let item = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: "spawn".to_string(),
        tool: CollabAgentTool::SpawnAgent,
        status: CollabAgentToolCallStatus::Completed,
        observe_commentary: Some(false),
        wake_on_completion: None,
        target_messages: Some(false),
        queue_input: Some(false),
        mailbox_input: None,
        deadline_at_ms: None,
        sender_thread_id: sender,
        receiver_thread_ids: vec![receiver],
        receiver_agents: vec![agent],
        prompt: Some("Implement authentication.".to_string()),
        model: None,
        reasoning_effort: None,
        agents_states: Default::default(),
        completion_presentation_agent_ids: None,
    });
    let expected = ThreadItem::from(item.clone());
    let completed = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: sender,
        turn_id: "turn".to_string(),
        item,
        started_at_ms: Some(1),
        completed_at_ms: 2,
    });
    let notification =
        item_event_to_server_notification(completed.clone(), &sender.to_string(), "turn");
    let wire = serde_json::to_value(notification).unwrap();
    assert_eq!(
        wire["params"]["item"]["receiverAgents"],
        json!([{
            "threadId": receiver.to_string(),
            "agentRef": "2",
            "taskPath": "/root/backend/auth",
            "agentNickname": "Pascal",
            "agentRole": "coder",
        }]),
    );
    let ServerNotification::ItemCompleted(notification) =
        serde_json::from_value::<ServerNotification>(wire).unwrap()
    else {
        panic!("expected item/completed");
    };
    assert_eq!(notification.item, expected);
    let rollout = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        })),
        RolloutItem::EventMsg(completed),
    ];
    let replay: Vec<RolloutItem> =
        serde_json::from_str(&serde_json::to_string(&rollout).unwrap()).unwrap();
    let turns = build_turns_from_rollout_items(&replay);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items, vec![expected]);
}

#[test]
fn legacy_agent_refs_omit_task_path_without_inference() {
    let thread_id = ThreadId::new();
    let core = CoreCollabAgentRef {
        thread_id,
        agent_ref: None,
        task_path: None,
        agent_nickname: Some("/root/not-a-task".to_string()),
        agent_role: None,
    };
    let wire = serde_json::to_value(&core).unwrap();
    assert!(wire.get("task_path").is_none());
    assert!(wire.get("agent_ref").is_none());
    assert_eq!(
        serde_json::from_value::<CoreCollabAgentRef>(wire).unwrap(),
        core,
    );
    let api = CollabAgentRef::from(core);
    let mut wire = serde_json::to_value(&api).unwrap();
    assert_eq!(
        wire.as_object_mut().unwrap().remove("taskPath"),
        Some(serde_json::Value::Null),
    );
    assert_eq!(
        wire.as_object_mut().unwrap().remove("agentRef"),
        Some(serde_json::Value::Null),
    );
    assert_eq!(serde_json::from_value::<CollabAgentRef>(wire).unwrap(), api);
}
