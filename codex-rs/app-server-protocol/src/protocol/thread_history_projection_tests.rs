use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::security_risk::SecurityRiskScore;
use codex_protocol::user_input::UserInput;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

use super::*;
use crate::build_turns_from_rollout_items;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnError;

#[test]
fn projects_turn_lifecycle_without_prior_builder_state() {
    let started = project(RolloutItem::EventMsg(EventMsg::TurnStarted(
        TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            trace_id: None,
            started_at: Some(10),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        },
    )));
    let completed = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(started.changed_turns.len(), 1);
    assert_eq!(started.changed_turns[0].turn_id, "turn-1");
    assert_eq!(started.changed_turns[0].status, TurnStatus::InProgress);
    assert_eq!(started.changed_turns[0].started_at, Some(10));
    assert_eq!(
        completed,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Completed,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_failed_turn_completion_as_snapshot() {
    let error = ErrorEvent {
        misalignment: None,
        message: "request failed".to_string(),
        codex_error_info: None,
    };

    let changes = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: Some(error),
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Failed,
                error: Some(TurnError {
                    misalignment: None,
                    message: "request failed".to_string(),
                    codex_error_info: None,
                    additional_details: None,
                }),
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_completed_canonical_turn_items() {
    let thread_id = ThreadId::default();
    let user_item = TurnItem::UserMessage(UserMessageItem {
        id: "user-1".to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
    });
    let agent_item = TurnItem::AgentMessage(AgentMessageItem {
        id: "agent-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: "done".to_string(),
        }],
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
        sub_agent_completion: None,
    });

    let user_changes = project(item_completed(thread_id, "turn-1", user_item.clone()));
    let agent_changes = project(item_completed(thread_id, "turn-1", agent_item.clone()));

    assert_eq!(
        user_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(user_item),
            started_at_ms: Some(100),
            completed_at_ms: Some(123),
        }]
    );
    assert_eq!(
        agent_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(agent_item),
            started_at_ms: Some(100),
            completed_at_ms: Some(123),
        }]
    );
}

#[test]
fn projects_optional_completed_item_lifecycle_timestamps() {
    let item = TurnItem::UserMessage(UserMessageItem {
        id: "user-1".to_string(),
        client_id: None,
        content: Vec::new(),
    });

    for (started_at_ms, completed_at_ms, expected_completed_at_ms) in
        [(None, 123, Some(123)), (Some(100), 0, None)]
    {
        let changes = project(RolloutItem::EventMsg(EventMsg::ItemCompleted(
            ItemCompletedEvent {
                thread_id: ThreadId::default(),
                turn_id: "turn-1".to_string(),
                item: item.clone(),
                started_at_ms,
                completed_at_ms,
            },
        )));

        assert_eq!(
            changes,
            ThreadHistoryChangeSet {
                changed_items: vec![ThreadHistoryItemChange {
                    turn_id: "turn-1".to_string(),
                    item: ThreadItem::from(item.clone()),
                    started_at_ms,
                    completed_at_ms: expected_completed_at_ms,
                }],
                ..Default::default()
            }
        );
    }
}

#[test]
fn projects_inter_agent_response_items_into_paginated_history() {
    let mut item = ResponseItem::AgentMessage {
        id: Some(ResponseItemId::with_suffix("amsg", "task")),
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "Inspect the repository.".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    item.set_turn_id_if_missing("turn-1");

    assert_eq!(
        project(RolloutItem::ResponseItem(item.into())),
        ThreadHistoryChangeSet {
            changed_items: vec![ThreadHistoryItemChange {
                turn_id: "turn-1".to_string(),
                item: ThreadItem::AgentMessage {
                    id: "amsg_task".to_string(),
                    text: "Agent message from `/root`:\n\nInspect the repository.".to_string(),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                },
                started_at_ms: None,
                completed_at_ms: None,
            }],
            ..Default::default()
        }
    );
}

#[test]
fn ignores_legacy_abort_without_turn_id_and_context_only_records() {
    let aborted = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: None,
            reason: TurnAbortReason::Interrupted,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    )));
    let compacted = project(RolloutItem::Compacted(CompactedItem {
        message: String::new(),
        replacement_history: None,
        guardian_history: None,
        mcp_resource_origins: None,
        compaction_summary_tokens: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        ..Default::default()
    }));
    let security_risk = project(RolloutItem::SecurityRiskScore(SecurityRiskScore {
        scores: BTreeMap::from([("action_risk".to_string(), 0.92)]),
        call_id: None,
        action: None,
        sampled_at: None,
    }));

    assert!(aborted.is_empty());
    assert!(compacted.is_empty());
    assert!(security_risk.is_empty());
}

#[test]
fn ignores_representation_only_compaction_repairs() {
    let turns = build_turns_from_rollout_items(&[RolloutItem::Compacted(CompactedItem {
        replacement_history_media_repair: true,
        ..Default::default()
    })]);

    assert!(turns.is_empty());
}

#[test]
fn projects_identified_turn_aborts() {
    let changes = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Interrupted,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

fn project(item: RolloutItem) -> ThreadHistoryChangeSet {
    project_rollout_line(&RolloutLine {
        timestamp: "2026-07-09T00:00:00.000Z".to_string(),
        ordinal: Some(7),
        item,
    })
}

fn item_completed(thread_id: ThreadId, turn_id: &str, item: TurnItem) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        started_at_ms: Some(100),
        completed_at_ms: 123,
    }))
}
