use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::build_world_state_from_turn_context;
use crate::session::tests::make_session_and_context;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::DynamicToolsSnapshot;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn response_item(item: ResponseItem) -> RolloutItem {
    RolloutItem::ResponseItem(item.into())
}

fn user_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn developer_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn inter_agent_msg(text: &str, trigger_turn: bool) -> ResponseItem {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        text.to_string(),
        trigger_turn,
    );
    communication.to_response_input_item().into()
}

fn inter_agent_communication(text: &str, trigger_turn: bool) -> RolloutItem {
    RolloutItem::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        text.to_string(),
        trigger_turn,
    ))
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_attribution: None,
        turn_id: turn_id.to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn turn_completed(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: turn_id.to_string(),
        started_at: None,
        last_agent_message: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[test]
fn truncates_rollout_after_terminal_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let truncated =
        truncate_rollout_after_turn_id(rollout.clone(), "turn-2").expect("truncate through turn-2");

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..4]).unwrap()
    );
}

#[test]
fn truncates_rollout_before_terminal_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
    ];

    let truncated =
        truncate_rollout_before_turn_id(rollout.clone(), "turn-2").expect("truncate before turn-2");
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..2]).unwrap()
    );
    assert!(
        truncate_rollout_before_turn_id(rollout, "turn-1")
            .expect("truncate before turn-1")
            .is_empty()
    );
}

#[test]
fn truncates_rollout_before_in_progress_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
    ];

    let truncated = truncate_rollout_before_turn_id(rollout.clone(), "turn-2")
        .expect("truncate before in-progress turn-2");

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..2]).unwrap()
    );
}

#[test]
fn truncate_rollout_before_turn_id_rejects_rolled_back_turn() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let err = truncate_rollout_before_turn_id(rollout, "turn-2")
        .expect_err("rolled-back turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "beforeTurnId 'turn-2' was not found in the source thread"
    ));
}

#[test]
fn truncate_rollout_before_turn_id_rejects_synthetic_legacy_turn_id() {
    let rollout = vec![RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "legacy".to_string(),
            ..Default::default()
        },
    ))];

    let err = truncate_rollout_before_turn_id(rollout, "rollout-0")
        .expect_err("synthetic turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message
                == "beforeTurnId 'rollout-0' is not a persisted canonical turn in the source thread"
    ));
}

#[test]
fn truncate_rollout_after_turn_id_rejects_rolled_back_turn() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let err = truncate_rollout_after_turn_id(rollout, "turn-2")
        .expect_err("rolled-back turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "lastTurnId 'turn-2' was not found in the source thread"
    ));
}

#[test]
fn fork_truncation_removes_exact_ranges_before_copying_rollout() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            rollback_start_index: Some(2),
            num_turns: 0,
            materialized_turns: None,
        })),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let truncated =
        truncate_rollout_after_turn_id(rollout, "turn-3").expect("truncate through turn-3");

    assert_eq!(
        serde_json::to_value(truncated).expect("serialize truncated rollout"),
        serde_json::to_value(vec![
            turn_started("turn-1"),
            turn_completed("turn-1"),
            turn_started("turn-3"),
            turn_completed("turn-3"),
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn truncate_rollout_after_turn_id_rejects_synthetic_legacy_turn_id() {
    let rollout = vec![RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "legacy".to_string(),
            ..Default::default()
        },
    ))];

    let err = truncate_rollout_after_turn_id(rollout, "rollout-0")
        .expect_err("synthetic turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message
                == "lastTurnId 'rollout-0' is not a persisted canonical turn in the source thread"
    ));
}

#[test]
fn truncate_rollout_after_turn_id_rejects_in_progress_turn() {
    let rollout = vec![turn_started("turn-1")];

    let err = truncate_rollout_after_turn_id(rollout, "turn-1")
        .expect_err("in-progress turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "lastTurnId 'turn-1' identifies an in-progress turn"
    ));
}

#[test]
fn truncates_rollout_from_start_before_nth_user_only() {
    let items = [
        user_msg("u1"),
        assistant_msg("a1"),
        assistant_msg("a2"),
        user_msg("u2"),
        assistant_msg("a3"),
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "1")),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "s".to_string(),
            }],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            status: None,
            encrypted_content: None,
            id: None,
            call_id: "c1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
        },
        assistant_msg("a4"),
    ];

    let rollout: Vec<RolloutItem> = items.iter().cloned().map(response_item).collect();

    let truncated = truncate_rollout_before_nth_user_message_from_start(
        rollout.clone(),
        /*n_from_start*/ 1,
    );
    let expected = vec![
        response_item(items[0].clone()),
        response_item(items[1].clone()),
        response_item(items[2].clone()),
    ];
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );

    let truncated2 = truncate_rollout_before_nth_user_message_from_start(
        rollout.clone(),
        /*n_from_start*/ 2,
    );
    assert_eq!(
        serde_json::to_value(&truncated2).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn truncation_max_keeps_full_rollout() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(user_msg("u2")),
    ];

    let truncated =
        truncate_rollout_before_nth_user_message_from_start(rollout.clone(), usize::MAX);

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn truncates_rollout_from_start_applies_thread_rollback_markers() {
    let rollout_items = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(user_msg("u2")),
        response_item(assistant_msg("a2")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        response_item(user_msg("u3")),
        response_item(assistant_msg("a3")),
        response_item(user_msg("u4")),
        response_item(assistant_msg("a4")),
    ];

    // Effective user history after applying rollback(1) is: u1, u3, u4.
    // So n_from_start=2 should cut before u4 (not u3).
    let truncated = truncate_rollout_before_nth_user_message_from_start(
        rollout_items.clone(),
        /*n_from_start*/ 2,
    );
    let expected = rollout_items[..7].to_vec();
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_counts_trigger_turn_messages() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(inter_agent_msg(
            "queued message",
            /*trigger_turn*/ false,
        )),
        response_item(assistant_msg("a2")),
        response_item(inter_agent_msg(
            "triggered task",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a3")),
        response_item(user_msg("u2")),
        response_item(assistant_msg("a4")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 2);
    let expected = rollout[4..].to_vec();

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn fork_turn_positions_use_inter_agent_delivery_metadata() {
    let rollout = vec![
        response_item(user_msg("user task")),
        inter_agent_communication("queued during user turn", /*trigger_turn*/ false),
        response_item(assistant_msg("first answer")),
        inter_agent_communication("follow-up task", /*trigger_turn*/ true),
        response_item(assistant_msg("second answer")),
        response_item(user_msg("next user task")),
    ];

    assert_eq!(fork_turn_positions_in_rollout(&rollout), vec![0, 3, 5]);
}

#[test]
fn instruction_positions_include_user_event_before_model_message() {
    let rollout = vec![
        turn_started("turn-1"),
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "initial".to_string(),
            ..Default::default()
        })),
        RolloutItem::ResponseItem(user_msg("initial").into()),
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "steer".to_string(),
            ..Default::default()
        })),
        RolloutItem::ResponseItem(user_msg("steer").into()),
    ];

    assert_eq!(instruction_positions_in_rollout(&rollout), vec![1, 3]);
}

#[test]
fn fork_turn_positions_use_canonical_agent_messages_and_delivery_metadata() {
    let queued = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "queued during user turn".to_string(),
        /*trigger_turn*/ false,
    );
    let triggered = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "follow-up task".to_string(),
        /*trigger_turn*/ true,
    );
    let mut rollout = vec![
        response_item(user_msg("user task")),
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        response_item(queued.to_model_input_item()),
        response_item(assistant_msg("first answer")),
        RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true },
        response_item(triggered.to_model_input_item()),
        response_item(assistant_msg("second answer")),
        response_item(user_msg("next user task")),
    ];

    assert_eq!(fork_turn_positions_in_rollout(&rollout), vec![0, 4, 7]);
    assert_eq!(instruction_positions_in_rollout(&rollout), vec![0, 1, 4, 7]);

    rollout.insert(
        7,
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
    );
    assert_eq!(fork_turn_positions_in_rollout(&rollout), vec![0, 8]);
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_drops_startup_prefix_even_when_under_limit() {
    let rollout = vec![
        response_item(developer_msg("startup developer context")),
        response_item(user_msg("current task")),
        response_item(assistant_msg("answer")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 2);
    let expected = rollout[1..].to_vec();

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

fn fork_catalog(name: &str) -> Vec<DynamicToolSpec> {
    let function = DynamicToolFunctionSpec {
        name: name.to_string(),
        description: "Resolve a selected resource".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"resource": {"type": "string", "enum": ["first", "second"]}},
            "required": ["resource"],
            "additionalProperties": false,
        }),
        defer_loading: true,
    };
    vec![
        DynamicToolSpec::Function(function.clone()),
        DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: "resources".to_string(),
            description: "Selected resource capabilities".to_string(),
            tools: vec![DynamicToolNamespaceTool::Function(function)],
        }),
    ]
}

fn fork_catalog_header(thread_id: ThreadId, tools: Option<Vec<DynamicToolSpec>>) -> RolloutItem {
    RolloutItem::SessionMeta(Box::new(SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            base_instructions: Some(BaseInstructions {
                text: "parent startup instructions".to_string(),
                provenance: None,
            }),
            dynamic_tools: tools,
            selected_capability_roots: vec![SelectedCapabilityRoot {
                id: "parent-capability".to_string(),
                location: CapabilityRootLocation::Environment {
                    environment_id: "parent-environment".to_string(),
                    path: codex_utils_path_uri::PathUri::parse("file:///parent/capability")
                        .expect("capability path"),
                },
            }],
            ..Default::default()
        },
        git: None,
    }))
}

fn fork_catalog_checkpoint(thread_id: ThreadId, tools: Vec<DynamicToolSpec>) -> CompactedItem {
    CompactedItem {
        message: "retained compaction summary".to_string(),
        replacement_history: Some(vec![assistant_msg("checkpoint context").into()]),
        compaction_summary_tokens: Some(37),
        window_number: Some(4),
        window_id: Some("checkpoint-window".to_string()),
        resume_metadata: Some(CompactionResumeMetadata {
            dynamic_tools: Some(Box::new(DynamicToolsSnapshot { thread_id, tools })),
            multi_agent_version: Some(MultiAgentVersion::V2),
            last_started_turn_id: Some("checkpoint-turn".to_string()),
            turn_attribution: None,
            previous_turn_settings: None,
        }),
        replacement_history_media_sanitized_prefix_len: Some(1),
        replacement_history_media_repair: true,
        ..Default::default()
    }
}

#[test]
fn last_n_fork_preserves_removed_checkpoint_catalog_without_removed_context() {
    let owner = ThreadId::new();
    let foreign = ThreadId::new();
    for tools in [fork_catalog("selected"), Vec::new()] {
        for header_after_boundary in [false, true] {
            let header = fork_catalog_header(owner, Some(fork_catalog("creation")));
            let kept_user = response_item(user_msg("retained task"));
            let kept_answer = response_item(assistant_msg("retained answer"));
            let mut rollout = vec![
                response_item(user_msg("earlier removed task")),
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                    num_turns: 1,
                    materialized_turns: None,
                    rollback_start_index: Some(0),
                })),
            ];
            if !header_after_boundary {
                rollout.push(header.clone());
            }
            rollout.push(kept_user.clone());
            if header_after_boundary {
                rollout.push(header.clone());
            }
            rollout.push(kept_answer.clone());
            let removed_start = rollout.len() as u64;
            rollout.extend([
                response_item(user_msg("removed task")),
                RolloutItem::Compacted(Box::new(fork_catalog_checkpoint(owner, tools.clone()))),
                fork_catalog_header(foreign, Some(fork_catalog("foreign header"))),
                RolloutItem::Compacted(Box::new(fork_catalog_checkpoint(
                    foreign,
                    fork_catalog("foreign checkpoint"),
                ))),
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                    num_turns: 1,
                    materialized_turns: None,
                    rollback_start_index: Some(removed_start),
                })),
            ]);

            let truncated = truncate_rollout_to_last_n_fork_turns(rollout, 1);
            let mut expected = vec![kept_user];
            if header_after_boundary {
                expected.push(header);
            }
            expected.extend([
                kept_answer,
                RolloutItem::DynamicToolsUpdated {
                    thread_id: owner,
                    tools: tools.clone(),
                },
            ]);
            assert_eq!(
                serde_json::to_value(&truncated).unwrap(),
                serde_json::to_value(&expected).unwrap(),
            );
            assert_eq!(latest_dynamic_tools(&truncated), Some(tools.as_slice()));
        }
    }
}

#[test]
fn exact_prefix_fork_keeps_only_catalog_metadata_inside_original_boundary() {
    let owner = ThreadId::new();
    for tools in [fork_catalog("selected"), Vec::new()] {
        let retained = vec![
            fork_catalog_header(owner, Some(fork_catalog("creation"))),
            turn_started("turn-1"),
            response_item(user_msg("retained task")),
            response_item(assistant_msg("retained answer")),
            turn_completed("turn-1"),
        ];
        let mut rollout = retained.clone();
        let removed_start = rollout.len() as u64;
        rollout.extend([
            turn_started("removed-turn"),
            response_item(user_msg("removed task")),
            RolloutItem::Compacted(Box::new(fork_catalog_checkpoint(owner, tools.clone()))),
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: Some(1),
                rollback_start_index: Some(removed_start),
            })),
            turn_started("turn-2"),
            response_item(user_msg("excluded future task")),
            turn_completed("turn-2"),
            RolloutItem::DynamicToolsUpdated {
                thread_id: owner,
                tools: fork_catalog("excluded future catalog"),
            },
        ]);
        let mut expected = retained;
        expected.push(RolloutItem::DynamicToolsUpdated {
            thread_id: owner,
            tools: tools.clone(),
        });

        let before = truncate_rollout_before_turn_id(rollout.clone(), "turn-2")
            .expect("cut before the surviving next turn");
        let after = truncate_rollout_after_turn_id(rollout, "turn-1")
            .expect("cut after the surviving previous turn");
        for truncated in [before, after] {
            assert_eq!(
                serde_json::to_value(&truncated).unwrap(),
                serde_json::to_value(&expected).unwrap(),
            );
            assert_eq!(latest_dynamic_tools(&truncated), Some(tools.as_slice()));
        }
    }
}

#[test]
fn last_n_fork_preserves_full_catalog_and_clear_without_restoring_startup_metadata() {
    let thread_id = ThreadId::new();
    for tools in [fork_catalog("selected"), Vec::new()] {
        for source in ["creation", "update", "checkpoint"] {
            let creation_tools = if source == "creation" {
                tools.clone()
            } else {
                fork_catalog("creation")
            };
            let mut rollout = vec![
                fork_catalog_header(thread_id, Some(creation_tools)),
                response_item(developer_msg("parent startup context")),
            ];
            match source {
                "update" => rollout.push(RolloutItem::DynamicToolsUpdated {
                    thread_id,
                    tools: tools.clone(),
                }),
                "checkpoint" => rollout.push(RolloutItem::Compacted(Box::new(
                    fork_catalog_checkpoint(thread_id, tools.clone()),
                ))),
                _ => {}
            }
            rollout.push(response_item(user_msg("old task")));
            rollout.push(response_item(assistant_msg("old answer")));
            let suffix = vec![
                response_item(user_msg("current task")),
                response_item(assistant_msg("current answer")),
            ];
            rollout.extend(suffix.clone());
            assert_eq!(latest_dynamic_tools(&rollout), Some(tools.as_slice()));

            let truncated = truncate_rollout_to_last_n_fork_turns(rollout, 1);
            let mut expected = suffix;
            expected.push(RolloutItem::DynamicToolsUpdated {
                thread_id,
                tools: tools.clone(),
            });
            assert_eq!(
                serde_json::to_value(&truncated).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            let history = InitialHistory::Forked(truncated);
            assert_eq!(history.get_dynamic_tools(), Some(tools.clone()));
            assert_eq!(history.get_base_instructions(), None);
            assert!(history.get_selected_capability_roots().is_empty());
            assert!(
                history
                    .get_rollout_items()
                    .iter()
                    .all(|item| !matches!(item, RolloutItem::SessionMeta(_)))
            );
        }
    }
}

#[test]
fn last_n_fork_rejects_later_headers_and_foreign_catalogs_without_clearing_absent_metadata() {
    let source_thread_id = ThreadId::new();
    let foreign_thread_id = ThreadId::new();
    for selected_tools in [None, Some(fork_catalog("selected")), Some(Vec::new())] {
        for later_header_id in [source_thread_id, foreign_thread_id] {
            let foreign_checkpoint =
                fork_catalog_checkpoint(foreign_thread_id, fork_catalog("foreign_checkpoint"));
            let mut expected_checkpoint = foreign_checkpoint.clone();
            expected_checkpoint
                .resume_metadata
                .as_mut()
                .unwrap()
                .dynamic_tools = None;
            let rollout = vec![
                fork_catalog_header(source_thread_id, selected_tools.clone()),
                response_item(user_msg("old task")),
                response_item(user_msg("current task")),
                fork_catalog_header(later_header_id, Some(fork_catalog("later_header"))),
                RolloutItem::DynamicToolsUpdated {
                    thread_id: foreign_thread_id,
                    tools: fork_catalog("foreign_update"),
                },
                RolloutItem::Compacted(Box::new(foreign_checkpoint)),
                response_item(assistant_msg("current answer")),
            ];
            assert_eq!(latest_dynamic_tools(&rollout), selected_tools.as_deref());
            let truncated = truncate_rollout_to_last_n_fork_turns(rollout, 1);
            let mut expected = vec![
                response_item(user_msg("current task")),
                RolloutItem::Compacted(Box::new(expected_checkpoint)),
                response_item(assistant_msg("current answer")),
            ];
            if let Some(tools) = &selected_tools {
                expected.push(RolloutItem::DynamicToolsUpdated {
                    thread_id: source_thread_id,
                    tools: tools.clone(),
                });
            }
            assert_eq!(
                serde_json::to_value(&truncated).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert_eq!(latest_dynamic_tools(&truncated), selected_tools.as_deref());
            let history = InitialHistory::Forked(truncated);
            assert_eq!(history.get_base_instructions(), None);
            assert!(history.get_selected_capability_roots().is_empty());
        }
    }
}

#[test]
fn last_n_fork_preserves_headerless_catalog_with_its_actual_latest_producer() {
    let older_thread_id = ThreadId::new();
    let selected_thread_id = ThreadId::new();
    for tools in [fork_catalog("selected"), Vec::new()] {
        for from_checkpoint in [false, true] {
            let selected_record = if from_checkpoint {
                RolloutItem::Compacted(Box::new(fork_catalog_checkpoint(
                    selected_thread_id,
                    tools.clone(),
                )))
            } else {
                RolloutItem::DynamicToolsUpdated {
                    thread_id: selected_thread_id,
                    tools: tools.clone(),
                }
            };
            let rollout = vec![
                RolloutItem::DynamicToolsUpdated {
                    thread_id: older_thread_id,
                    tools: fork_catalog("older"),
                },
                selected_record,
                response_item(user_msg("old task")),
                response_item(user_msg("current task")),
                response_item(assistant_msg("current answer")),
            ];
            let expected = vec![
                response_item(user_msg("current task")),
                response_item(assistant_msg("current answer")),
                RolloutItem::DynamicToolsUpdated {
                    thread_id: selected_thread_id,
                    tools: tools.clone(),
                },
            ];
            let truncated = truncate_rollout_to_last_n_fork_turns(rollout, 1);
            assert_eq!(
                serde_json::to_value(&truncated).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert_eq!(latest_dynamic_tools(&truncated), Some(tools.as_slice()));
        }
    }
}

#[test]
fn last_n_fork_keeps_surviving_canonical_header_and_catalog_without_duplicate_carrier() {
    let thread_id = ThreadId::new();
    let mut expected = vec![
        response_item(user_msg("current task")),
        fork_catalog_header(thread_id, Some(fork_catalog("creation"))),
        RolloutItem::DynamicToolsUpdated {
            thread_id,
            tools: Vec::new(),
        },
        response_item(assistant_msg("current answer")),
    ];
    let mut rollout = expected.clone();
    rollout.insert(
        2,
        fork_catalog_header(thread_id, Some(fork_catalog("later"))),
    );
    let truncated = truncate_rollout_to_last_n_fork_turns(rollout, 1);
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    let empty_catalog: &[DynamicToolSpec] = &[];
    assert_eq!(latest_dynamic_tools(&truncated), Some(empty_catalog));

    // Catalog metadata alone must not turn a fresh or entirely rolled-back fork into history.
    assert!(truncate_rollout_to_last_n_fork_turns(expected.clone(), 0).is_empty());
    expected.remove(0);
    assert!(truncate_rollout_to_last_n_fork_turns(expected.clone(), 1).is_empty());
    expected.insert(0, response_item(user_msg("rolled-back task")));
    expected.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        },
    )));
    assert!(truncate_rollout_to_last_n_fork_turns(expected, 1).is_empty());
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_applies_thread_rollback_markers() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(inter_agent_msg(
            "triggered task",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a2")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        response_item(user_msg("u2")),
        response_item(assistant_msg("a3")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 2);

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn fork_turn_positions_ignore_zero_turn_rollback_markers() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(inter_agent_msg(
            "triggered task",
            /*trigger_turn*/ true,
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        response_item(user_msg("u2")),
    ];

    assert_eq!(fork_turn_positions_in_rollout(&rollout), vec![0, 1, 3]);
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_discards_trigger_boundaries_in_rolled_back_suffix() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(user_msg("u2")),
        response_item(inter_agent_msg(
            "triggered task",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a1")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        response_item(user_msg("u3")),
        response_item(assistant_msg("a2")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 2);

    let expected = rollout[1..].to_vec();

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_discards_rolled_back_assistant_instruction_turns() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(inter_agent_msg(
            "triggered task 1",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a2")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
        response_item(inter_agent_msg(
            "triggered task 2",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a3")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 1);
    let expected = rollout[5..].to_vec();

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn truncates_rollout_to_last_n_fork_turns_keeps_full_rollout_when_n_is_large() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(inter_agent_msg(
            "triggered task",
            /*trigger_turn*/ true,
        )),
        response_item(assistant_msg("a2")),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(rollout.clone(), /*n_from_end*/ 10);

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn counted_fork_uses_positions_when_user_text_repeats() {
    let rollout = vec![
        response_item(user_msg("repeat the check")),
        response_item(assistant_msg("old result")),
        response_item(user_msg("repeat the check")),
        response_item(assistant_msg("current result")),
    ];
    let expected = rollout[2..].to_vec();
    let selected = truncate_rollout_to_last_n_fork_turns(rollout, /*n_from_end*/ 1);
    assert_eq!(
        serde_json::to_value(selected).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[tokio::test]
async fn ignores_session_prefix_messages_when_truncating_rollout_from_start() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let step_context = StepContext::for_test(turn_context);
    let updates = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await
        .0;
    let mut items = crate::context_manager::updates::merge_world_state_updates(updates);
    items.push(user_msg("feature request"));
    items.push(assistant_msg("ack"));
    items.push(user_msg("second question"));
    items.push(assistant_msg("answer"));

    let rollout_items: Vec<RolloutItem> = items.iter().cloned().map(response_item).collect();

    let truncated =
        truncate_rollout_before_nth_user_message_from_start(rollout_items, /*n_from_start*/ 1);
    let expected: Vec<RolloutItem> = vec![
        response_item(items[0].clone()),
        response_item(items[1].clone()),
        response_item(items[2].clone()),
        response_item(items[3].clone()),
    ];

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}
