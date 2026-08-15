use std::collections::HashSet;

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn agent_observation_context_is_not_a_rollback_turn_boundary() {
    let response = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<subagent_commentary>\ncommentary\n</subagent_commentary>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert!(!rollback::counts_as_boundary(&response));
    assert!(rollback::is_pre_turn_context_update(&response));
}

#[test]
fn user_agent_control_audit_survives_rollback_of_preceding_turn() {
    let thread_id = ThreadId::new();
    let user = line(RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "original turn".to_string(),
            ..Default::default()
        },
    )));
    let control = line(RolloutItem::EventMsg(EventMsg::ItemCompleted(
        ItemCompletedEvent {
            thread_id,
            turn_id: "user-control-1".to_string(),
            item: TurnItem::UserAgentControl(UserAgentControlItem::succeeded(
                UserAgentControlAction::Prompt,
            )),
            started_at_ms: Some(1),
            completed_at_ms: 1,
        },
    )));
    let rollback = line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: None,
        },
    )));
    let lines = [user, control, rollback];
    let mut planner = RollbackPlanner::new();
    for line in &lines {
        planner.observe(line).expect("observe rollout line");
    }
    let plan = planner.finish();

    assert!(
        plan.apply(0, lines[0].clone())
            .expect("apply user")
            .is_none()
    );
    assert!(
        plan.apply(1, lines[1].clone())
            .expect("apply control")
            .is_some()
    );
    assert!(
        plan.apply(2, lines[2].clone())
            .expect("apply rollback marker")
            .is_none()
    );
}

#[test]
fn user_agent_task_context_survives_rollback_of_surrounding_turn() {
    let user = line(RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "original turn".to_string(),
            ..Default::default()
        },
    )));
    let task = line(RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: Some(new_user_agent_task_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<user_agent_task>delegated task</user_agent_task>".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    let rollback = line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: None,
        },
    )));
    let lines = [user, task, rollback];
    let mut planner = RollbackPlanner::new();
    for line in &lines {
        planner.observe(line).expect("observe rollout line");
    }
    let plan = planner.finish();

    assert!(
        plan.apply(0, lines[0].clone())
            .expect("apply user")
            .is_none()
    );
    assert!(
        plan.apply(1, lines[1].clone())
            .expect("apply task")
            .is_some()
    );
    assert!(
        plan.apply(2, lines[2].clone())
            .expect("apply rollback marker")
            .is_none()
    );
}

#[test]
fn agent_response_observation_survives_rollback_of_surrounding_turn() {
    let user = line(RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "original turn".to_string(),
            ..Default::default()
        },
    )));
    let observation = line(RolloutItem::AgentResponseObservation(
        AgentResponseObservation {
            observer_thread_id: ThreadId::new(),
            target_thread_id: ThreadId::new(),
            target_turn_id: Some("target-turn".to_string()),
            task_preview: Some("review the change".to_string()),
            promoted_task_context: None,
            pending_commentary: true,
            commentary_after_sequences: vec![7],
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery: AgentResponseFinalDelivery::Passive,
            final_delivery: AgentResponseFinalDelivery::Wake,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        },
    ));
    let rollback = line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: None,
        },
    )));
    let lines = [user, observation, rollback];
    let mut planner = RollbackPlanner::new();
    for line in &lines {
        planner.observe(line).expect("observe rollout line");
    }
    let plan = planner.finish();

    assert!(
        plan.apply(0, lines[0].clone())
            .expect("apply user")
            .is_none()
    );
    assert!(
        plan.apply(1, lines[1].clone())
            .expect("apply observation")
            .is_some()
    );
    assert!(
        plan.apply(2, lines[2].clone())
            .expect("apply rollback marker")
            .is_none()
    );
}

#[test]
fn committed_agent_response_pair_survives_historical_rollback() {
    let response_item_id = ResponseItemId::with_suffix("amsg", "committed");
    let user = line(RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "original turn".to_string(),
            ..Default::default()
        },
    )));
    let metadata = line(RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true });
    let response = line(RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: Some(response_item_id.clone()),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "committed response".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    let observation = line(RolloutItem::AgentResponseObservation(
        AgentResponseObservation {
            observer_thread_id: ThreadId::new(),
            target_thread_id: ThreadId::new(),
            target_turn_id: Some("target-turn".to_string()),
            task_preview: None,
            promoted_task_context: None,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery: AgentResponseFinalDelivery::Passive,
            final_delivery: AgentResponseFinalDelivery::Wake,
            final_delivery_response_item_id: Some(response_item_id.clone()),
            committed_delivery_response_item_ids: vec![response_item_id],
        },
    ));
    let rollback = line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: None,
        },
    )));
    let lines = [
        user.clone(),
        metadata.clone(),
        response.clone(),
        observation.clone(),
        rollback,
    ];
    let mut planner = RollbackPlanner::new();
    for line in &lines {
        planner.observe(line).expect("observe rollout line");
    }
    let plan = planner.finish();
    let retained = lines
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| plan.apply(index, line).expect("apply rollback plan"))
        .collect::<Vec<_>>();

    assert_eq!(
        serde_json::to_value(retained).expect("serialize retained rollout"),
        serde_json::to_value([user, metadata, response, observation])
            .expect("serialize expected rollout"),
    );
}

#[test]
fn late_observation_preserves_committed_response_in_rewritten_compaction() {
    let response_item_id = ResponseItemId::with_suffix("amsg", "late-commit");
    let committed_response = ResponseItem::AgentMessage {
        id: Some(response_item_id.clone()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "committed response".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let metadata = line(RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true });
    let response = line(RolloutItem::ResponseItem(committed_response.clone().into()));
    let compaction = line(RolloutItem::Compacted(CompactedItem {
        message: "checkpoint".to_string(),
        replacement_history: Some(vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "remove question".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
            committed_response.clone().into(),
        ]),
        ..Default::default()
    }));
    let rollback = line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: None,
        },
    )));
    let observation = line(RolloutItem::AgentResponseObservation(
        AgentResponseObservation {
            observer_thread_id: ThreadId::new(),
            target_thread_id: ThreadId::new(),
            target_turn_id: Some("target-turn".to_string()),
            task_preview: None,
            promoted_task_context: None,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery: AgentResponseFinalDelivery::Passive,
            final_delivery: AgentResponseFinalDelivery::Wake,
            final_delivery_response_item_id: Some(response_item_id.clone()),
            committed_delivery_response_item_ids: vec![response_item_id],
        },
    ));
    let lines = [metadata, response, compaction, rollback, observation];
    let mut planner = RollbackPlanner::new();
    for line in &lines {
        planner.observe(line).expect("observe rollout line");
    }
    let plan = planner.finish();
    let replacement_history = lines
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| plan.apply(index, line).expect("apply rollback plan"))
        .find_map(|line| match line.item {
            RolloutItem::Compacted(item) => item.replacement_history,
            _ => None,
        })
        .expect("retained compaction");

    assert_eq!(replacement_history, vec![committed_response.into()]);
}

#[test]
fn compaction_rollback_retains_committed_response_at_cut() {
    let response_item_id = ResponseItemId::with_suffix("amsg", "rollback-cut");
    let committed_response = ResponseItem::AgentMessage {
        id: Some(response_item_id.clone()),
        author: "/root/worker".to_string(),
        recipient: "/root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "committed response".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let kept_user = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "kept question".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut history = vec![
        kept_user.clone(),
        committed_response.clone(),
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "removed question".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    rollback::drop_last_n_user_turns(
        &mut history,
        /*num_turns*/ 2,
        &HashSet::from([response_item_id]),
    );

    assert_eq!(history, vec![kept_user, committed_response]);
}

fn line(item: RolloutItem) -> RolloutLine {
    RolloutLine {
        timestamp: "2026-08-12T00:00:00Z".to_string(),
        ordinal: None,
        item,
    }
}
