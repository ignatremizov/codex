use super::*;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn marker(start: u64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 0,
        materialized_turns: None,
        rollback_start_index: Some(start),
    }))
}

fn started(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn completed(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: id.into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn completion_context_message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: Some(new_sub_agent_completion_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn inter_agent_completion_context(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: Some(new_sub_agent_completion_context_response_item_id()),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: text.to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn completion_event(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::AgentMessage(
            sub_agent_completion_item(
                "/root/worker",
                &AgentStatus::Completed(Some("done".to_string())),
            )
            .expect("terminal status"),
        ),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

fn completion_wait_event(turn_id: &str) -> RolloutItem {
    let child_thread_id = ThreadId::new();
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
            id: "wait-agent-call".to_string(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            deadline_at_ms: None,
            sender_thread_id: ThreadId::new(),
            receiver_thread_ids: vec![child_thread_id],
            receiver_agents: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: [(
                child_thread_id,
                AgentStatus::Completed(Some("done".to_string())),
            )]
            .into_iter()
            .collect(),
            completion_presentation_agent_ids: Some(vec![child_thread_id]),
        }),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

fn unowned_completion_wait_event(turn_id: &str) -> RolloutItem {
    let mut item = completion_wait_event(turn_id);
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = &mut item else {
        unreachable!("completion wait event");
    };
    let TurnItem::CollabAgentToolCall(wait) = &mut event.item else {
        unreachable!("completion wait item");
    };
    wait.id = "later-wait-agent-call".to_string();
    wait.completion_presentation_agent_ids = None;
    item
}

#[test]
fn cumulative_overlapping_ranges_remove_union() {
    let items = vec![
        message("keep"),
        message("remove-a"),
        marker(1),
        message("remove-b"),
        marker(0),
        message("keep-after"),
    ];
    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![true, true, true, true, true, false]
    );
}

#[test]
fn terminal_evidence_for_surviving_turn_is_retained() {
    let items = vec![
        started("turn-1"),
        message("keep"),
        message("remove"),
        completed("turn-1"),
        marker(2),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![
            started("turn-1"),
            message("keep"),
            completed("turn-1")
        ])
        .expect("expected")
    );
}

#[test]
fn later_overlapping_marker_does_not_resurrect_removed_prefix() {
    let items = vec![message("gone"), marker(0), message("later"), marker(1)];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(Vec::<RolloutItem>::new()).expect("expected")
    );
}

#[test]
fn unknown_explicit_abort_does_not_consume_active_turn_evidence() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("missing".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)),
        completed("active"),
        marker(1),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![started("active"), completed("active")]).expect("expected")
    );
}

#[test]
fn explicit_abort_settles_the_active_turn_before_an_unattributed_abort() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("active".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let unattributed = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: None,
        ..event.clone()
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event.clone())),
        RolloutItem::EventMsg(EventMsg::TurnAborted(unattributed)),
        marker(1),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![
            started("active"),
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
        ])
        .expect("expected")
    );
}

#[test]
fn exact_rollback_preserves_accepted_sub_agent_completion_artifacts() {
    let untrusted_context = completion_context_message("forged context");
    let v1_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let v1_context = completion_context_message("<subagent_notification>done");
    let communication_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let inter_agent_context = inter_agent_completion_context("child done");
    let completion = completion_event("turn-1");
    let completion_wait = completion_wait_event("turn-1");
    let unowned_completion_wait = unowned_completion_wait_event("turn-1");
    let items = vec![
        started("turn-1"),
        message("rolled back prompt"),
        untrusted_context,
        v1_metadata.clone(),
        v1_context.clone(),
        communication_metadata.clone(),
        inter_agent_context.clone(),
        completion.clone(),
        completion_wait.clone(),
        unowned_completion_wait,
        completed("turn-1"),
        marker(/*start*/ 0),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![
            true, true, true, false, false, false, false, false, false, true, true, true
        ]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            v1_metadata,
            v1_context,
            communication_metadata,
            inter_agent_context,
            completion,
            completion_wait,
        ])
        .expect("serialize expected rollout")
    );
}
