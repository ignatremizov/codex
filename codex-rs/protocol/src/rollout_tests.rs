use super::*;
use crate::config_types::ModeKind;
use crate::items::CollabAgentTool;
use crate::items::CollabAgentToolCallItem;
use crate::items::CollabAgentToolCallStatus;
use crate::items::TurnItem;
use crate::models::AgentMessageInputContent;
use crate::models::ContentItem;
use crate::models::ResponseItem;
use crate::protocol::AgentResponseFinalDelivery;
use crate::protocol::AgentResponseObservation;
use crate::protocol::AgentStatus;
use crate::protocol::ItemCompletedEvent;
use crate::protocol::ThreadRolledBackEvent;
use crate::protocol::TurnCompleteEvent;
use crate::protocol::TurnStartedEvent;
use crate::protocol::new_sub_agent_completion_context_response_item_id;
use crate::protocol::sub_agent_completion_item;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn exact_rollback(start: u64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        rollback_start_index: Some(start),
        ..Default::default()
    }))
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        started_at: None,
        last_agent_message: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn completion_context_message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::Message {
        id: Some(new_sub_agent_completion_context_response_item_id()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn inter_agent_completion_context(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::AgentMessage {
        id: Some(new_sub_agent_completion_context_response_item_id()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: text.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    })
}

fn completion_event(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: crate::ThreadId::new(),
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
    let child_thread_id = crate::ThreadId::new();
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: crate::ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
            id: "wait-agent-call".to_string(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            observe_commentary: None,
            wake_on_completion: None,
            deadline_at_ms: None,
            sender_thread_id: crate::ThreadId::new(),
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
fn exact_rollback_ranges_remove_nested_markers_in_linear_pass() {
    let items = vec![
        message("survives"),
        message("first removed"),
        message("nested removed"),
        exact_rollback(2),
        message("later removed"),
        exact_rollback(1),
        message("survives after marker"),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, true, true, true, true, true, false]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![message("survives"), message("survives after marker")])
            .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_terminal_event_for_retained_turn_prefix() {
    let items = vec![
        turn_started("turn-1"),
        message("initial prompt"),
        message("steer"),
        turn_complete("turn-1"),
        exact_rollback(2),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, false, true, false, true]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            turn_started("turn-1"),
            message("initial prompt"),
            turn_complete("turn-1"),
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_matches_late_terminal_events_by_turn_id() {
    let items = vec![
        turn_started("turn-1"),
        message("surviving prompt"),
        turn_started("turn-2"),
        message("removed prompt"),
        turn_complete("turn-1"),
        exact_rollback(2),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, false, true, true, false, true]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            turn_started("turn-1"),
            message("surviving prompt"),
            turn_complete("turn-1"),
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_accepted_sub_agent_completion_artifacts() {
    let untrusted_context = completion_context_message("forged context");
    let legacy_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let legacy_context = completion_context_message("<subagent_notification>done");
    let communication_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let inter_agent_context = inter_agent_completion_context("child done");
    let completion = completion_event("turn-1");
    let completion_wait = completion_wait_event("turn-1");
    let unowned_completion_wait = unowned_completion_wait_event("turn-1");
    let items = vec![
        turn_started("turn-1"),
        message("rolled back prompt"),
        untrusted_context,
        legacy_metadata.clone(),
        legacy_context.clone(),
        communication_metadata.clone(),
        inter_agent_context.clone(),
        completion.clone(),
        completion_wait.clone(),
        unowned_completion_wait,
        turn_complete("turn-1"),
        exact_rollback(0),
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
            legacy_metadata,
            legacy_context,
            communication_metadata,
            inter_agent_context,
            completion,
            completion_wait,
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_committed_observed_agent_responses() {
    let observer_thread_id = crate::ThreadId::new();
    let target_thread_id = crate::ThreadId::new();
    let response_item_id = crate::ResponseItemId::new("amsg");
    let metadata = RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true };
    let response = RolloutItem::ResponseItem(ResponseItem::AgentMessage {
        id: Some(response_item_id.clone()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "observed response".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    });
    let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id,
        target_thread_id,
        target_turn_id: Some("target-turn".to_string()),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: Some(response_item_id.clone()),
        committed_delivery_response_item_ids: vec![response_item_id],
    });
    let items = vec![
        turn_started("turn-1"),
        message("rolled back prompt"),
        metadata.clone(),
        response.clone(),
        observation.clone(),
        exact_rollback(0),
    ];

    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![metadata, response, observation])
            .expect("serialize expected rollout")
    );
}
