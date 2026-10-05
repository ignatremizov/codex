use super::*;
use crate::CompactedItem;
use crate::CompactionResumeMetadata;
use crate::DynamicToolsSnapshot;
use crate::latest_dynamic_tools;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
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
        turn_attribution: None,
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
        root_turn_id: None,
        turn_id: id.into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
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
        rollout_without_exact_rollback_ranges(&items),
        vec![started("turn-1"), message("keep"), completed("turn-1")]
    );
}

#[test]
fn later_overlapping_marker_does_not_resurrect_removed_prefix() {
    let items = vec![message("gone"), marker(0), message("later"), marker(1)];
    assert_eq!(
        rollout_without_exact_rollback_ranges(&items),
        Vec::<RolloutItem>::new()
    );
}

#[test]
fn unknown_explicit_abort_does_not_consume_active_turn_evidence() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        root_turn_id: None,
        turn_id: Some("missing".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        error: None,
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
        rollout_without_exact_rollback_ranges(&items),
        vec![started("active"), completed("active")]
    );
}

#[test]
fn explicit_abort_settles_the_active_turn_before_an_unattributed_abort() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        root_turn_id: None,
        turn_id: Some("active".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        error: None,
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
        rollout_without_exact_rollback_ranges(&items),
        vec![
            started("active"),
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
        ]
    );
}

fn catalog(name: &str) -> Vec<DynamicToolSpec> {
    let function = DynamicToolFunctionSpec {
        name: name.to_string(),
        description: format!("Resolve {name} resources"),
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
            name: "resources".into(),
            description: "Full selected resource catalog".into(),
            tools: vec![DynamicToolNamespaceTool::Function(function)],
        }),
    ]
}

#[test]
fn exact_rollback_preserves_catalog_metadata_without_removed_checkpoint_context() {
    let thread_id = ThreadId::new();
    let foreign_thread_id = ThreadId::new();
    for tools in [catalog("replacement"), Vec::new()] {
        for from_checkpoint in [false, true] {
            let header = RolloutItem::SessionMeta(Box::new(SessionMetaLine {
                meta: SessionMeta {
                    session_id: thread_id.into(),
                    id: thread_id,
                    dynamic_tools: Some(catalog("creation")),
                    ..Default::default()
                },
                git: None,
            }));
            let update = RolloutItem::DynamicToolsUpdated {
                thread_id,
                tools: tools.clone(),
            };
            let catalog_record = if from_checkpoint {
                RolloutItem::Compacted(Box::new(CompactedItem {
                    message: "removed summary".into(),
                    replacement_history: Some(vec![
                        ResponseItem::Message {
                            id: None,
                            status: None,
                            encrypted_content: None,
                            role: "user".into(),
                            content: vec![ContentItem::InputText {
                                text: "removed context".into(),
                            }],
                            phase: None,
                            internal_chat_message_metadata_passthrough: None,
                        }
                        .into(),
                    ]),
                    resume_metadata: Some(CompactionResumeMetadata {
                        dynamic_tools: Some(Box::new(DynamicToolsSnapshot {
                            thread_id,
                            tools: tools.clone(),
                        })),
                        multi_agent_version: Some(codex_protocol::protocol::MultiAgentVersion::V2),
                        last_started_turn_id: Some("removed-turn".into()),
                        turn_attribution: None,
                        previous_turn_settings: None,
                    }),
                    compaction_summary_tokens: Some(42),
                    window_number: Some(7),
                    replacement_history_media_sanitized_prefix_len: Some(1),
                    replacement_history_media_repair: true,
                    ..Default::default()
                }))
            } else {
                update.clone()
            };
            let foreign = RolloutItem::DynamicToolsUpdated {
                thread_id: foreign_thread_id,
                tools: catalog("foreign"),
            };
            let mut items = vec![
                header.clone(),
                started("turn-1"),
                message("keep"),
                message("remove"),
                catalog_record,
                foreign.clone(),
                completed("turn-1"),
                marker(3),
            ];
            assert_eq!(
                exact_rollback_removed_items(&items),
                vec![
                    false,
                    false,
                    false,
                    true,
                    from_checkpoint,
                    false,
                    false,
                    true
                ]
            );
            let mut expected = vec![
                header,
                started("turn-1"),
                message("keep"),
                update,
                foreign,
                completed("turn-1"),
            ];
            let filtered = rollout_without_exact_rollback_ranges(&items);
            assert_eq!(
                serde_json::to_value(&filtered).expect("serialize retained history"),
                serde_json::to_value(&expected).expect("serialize expected history"),
            );
            assert_eq!(latest_dynamic_tools(&filtered), Some(tools.as_slice()));
            assert!(
                filtered
                    .iter()
                    .all(|item| !matches!(item, RolloutItem::Compacted(_)))
            );

            let later_tools = if tools.is_empty() {
                catalog("later")
            } else {
                Vec::new()
            };
            let later = RolloutItem::DynamicToolsUpdated {
                thread_id,
                tools: later_tools.clone(),
            };
            items.push(later.clone());
            expected.push(later);
            let filtered = rollout_without_exact_rollback_ranges(&items);
            assert_eq!(
                serde_json::to_value(&filtered).expect("serialize retained history"),
                serde_json::to_value(&expected).expect("serialize expected history"),
            );
            assert_eq!(
                latest_dynamic_tools(&filtered),
                Some(later_tools.as_slice())
            );
        }
    }
}
