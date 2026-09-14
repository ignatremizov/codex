use codex_history::CompactionResumeMetadata;
use codex_history::DynamicToolsSnapshot;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;

use super::super::MAX_ROLLOUT_LINE_BYTES;
use super::super::RolloutMigrationRateLimiter;
use super::prepare;
use super::select;

fn line(item: RolloutItem) -> RolloutLine {
    RolloutLine {
        timestamp: "2025-01-03T12:00:00Z".to_string(),
        ordinal: None,
        item,
    }
}

fn user(message: &str) -> RolloutLine {
    line(RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: message.to_string(),
            ..Default::default()
        },
    )))
}

fn exact(start: u64, num_turns: u32) -> RolloutLine {
    line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns,
            materialized_turns: Some(1),
            rollback_start_index: Some(start),
        },
    )))
}

fn catalog(name: &str) -> Vec<DynamicToolSpec> {
    vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: name.into(),
        description: format!("Resolve {name} resources"),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"resource": {"type": "string"}},
            "required": ["resource"],
            "additionalProperties": false,
        }),
        defer_loading: true,
    })]
}

fn catalog_checkpoint(thread_id: ThreadId, tools: Vec<DynamicToolSpec>) -> CompactedItem {
    CompactedItem {
        message: "removed checkpoint".into(),
        resume_metadata: Some(CompactionResumeMetadata {
            dynamic_tools: Some(Box::new(DynamicToolsSnapshot { thread_id, tools })),
            multi_agent_version: Some(codex_protocol::protocol::MultiAgentVersion::V2),
            last_started_turn_id: Some("removed-turn".into()),
            turn_attribution: None,
            previous_turn_settings: None,
        }),
        compaction_summary_tokens: Some(31),
        window_number: Some(4),
        ..Default::default()
    }
}

#[test]
fn zero_instruction_exact_rollback_removes_compaction_only_suffix() {
    let thread_id = ThreadId::new();
    for tools in [None, Some(catalog("replacement")), Some(Vec::new())] {
        let retained = user("retained");
        let compacted = tools.clone().map_or_else(
            || CompactedItem {
                message: "removed checkpoint".to_string(),
                ..Default::default()
            },
            |tools| catalog_checkpoint(thread_id, tools),
        );
        let mut checkpoint_line = line(RolloutItem::Compacted(Box::new(compacted)));
        checkpoint_line.timestamp = "2025-01-03T12:01:02Z".into();
        checkpoint_line.ordinal = Some(37);
        let mut expected = vec![retained.clone()];
        if let Some(tools) = tools {
            expected.push(RolloutLine {
                timestamp: checkpoint_line.timestamp.clone(),
                ordinal: checkpoint_line.ordinal,
                item: RolloutItem::DynamicToolsUpdated { thread_id, tools },
            });
        }
        let lines = [
            retained,
            checkpoint_line,
            exact(/*start*/ 1, /*num_turns*/ 0),
        ];
        let canonical = lines.iter().map(|line| line.item.clone()).collect();
        let records = lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| (index, Some(index), line))
            .collect();

        assert_eq!(
            serde_json::to_value(
                select(canonical, vec![0, 1, 2], records).expect("select exact survivors")
            )
            .expect("serialize survivors"),
            serde_json::to_value(expected).expect("serialize expected")
        );
    }
}

#[test]
fn exact_markers_do_not_reapply_instruction_counts_in_mixed_history() {
    let retained = user("retained");
    let lines = [
        retained.clone(),
        user("count removed"),
        line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        ))),
        user("exact removed"),
        exact(/*start*/ 3, /*num_turns*/ 1),
    ];
    let canonical = lines.iter().map(|line| line.item.clone()).collect();
    let records = lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| (index, Some(index), line))
        .collect();

    assert_eq!(
        serde_json::to_value(
            select(canonical, vec![0, 1, 2, 3, 4], records).expect("select mixed survivors")
        )
        .expect("serialize survivors"),
        serde_json::to_value(vec![retained]).expect("serialize expected")
    );
}

#[test]
fn normalized_only_records_follow_physical_source_intervals() {
    let retained = user("retained canonical");
    let compatibility_before = user("retained compatibility");
    let removed = user("removed canonical");
    let compatibility_inside = user("removed compatibility");
    let marker = exact(/*start*/ 1, /*num_turns*/ 0);
    let canonical = vec![
        retained.item.clone(),
        removed.item.clone(),
        marker.item.clone(),
    ];
    let records = vec![
        (0, Some(0), retained.clone()),
        (2, None, compatibility_before.clone()),
        (5, Some(1), removed),
        (6, None, compatibility_inside),
        (9, Some(2), marker),
    ];

    assert_eq!(
        serde_json::to_value(
            select(canonical, vec![0, 5, 9], records).expect("map source positions")
        )
        .expect("serialize survivors"),
        serde_json::to_value(vec![retained, compatibility_before]).expect("serialize expected")
    );
}

#[test]
fn cutoff_at_or_after_marker_is_rejected_before_replay() {
    for start in [0, 100] {
        let marker = exact(start, /*num_turns*/ 0);
        assert!(
            select(
                vec![marker.item.clone()],
                vec![0],
                vec![(0, Some(0), marker)],
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn count_only_detection_skips_invalid_utf8_records() {
    let home = tempfile::TempDir::new().expect("temporary directory");
    let path = home.path().join("rollout.jsonl");
    let mut bytes = b"\xff\n".to_vec();
    bytes.extend(serde_json::to_vec(&user("retained")).expect("serialize valid record"));
    std::fs::write(&path, bytes).expect("write invalid UTF-8 source");
    let mut limiter =
        RolloutMigrationRateLimiter::new(/*max_mib_per_second*/ None).expect("unlimited migration");

    assert!(
        prepare(&path, &mut limiter)
            .await
            .expect("detection")
            .is_none()
    );
}

#[tokio::test]
async fn oversized_canonical_record_keeps_its_decoded_coordinate_and_payload() {
    let home = tempfile::TempDir::new().expect("temporary directory");
    let path = home.path().join("rollout.jsonl");
    let mut retained = user(&"x".repeat(MAX_ROLLOUT_LINE_BYTES + 1));
    retained.timestamp = "2025-01-03T11:59:00Z".to_string();
    let removed = user("removed");
    let marker = exact(/*start*/ 1, /*num_turns*/ 0);
    let mut bytes = Vec::new();
    for record in [&retained, &removed, &marker] {
        serde_json::to_writer(&mut bytes, record).expect("serialize record");
        bytes.push(b'\n');
    }
    std::fs::write(&path, bytes).expect("write oversized source");
    let mut limiter =
        RolloutMigrationRateLimiter::new(/*max_mib_per_second*/ None).expect("unlimited migration");
    let selected = prepare(&path, &mut limiter)
        .await
        .expect("prepare exact source")
        .expect("exact marker detected");

    assert_eq!(
        serde_json::to_value(selected).expect("serialize selected history"),
        serde_json::to_value(vec![retained]).expect("serialize expected history")
    );
}

#[tokio::test]
async fn exact_source_with_invalid_utf8_is_rejected_by_canonical_reader() {
    let home = tempfile::TempDir::new().expect("temporary directory");
    let path = home.path().join("rollout.jsonl");
    let mut bytes = b"\xff\n".to_vec();
    for record in [user("removed"), exact(/*start*/ 0, /*num_turns*/ 0)] {
        serde_json::to_writer(&mut bytes, &record).expect("serialize record");
        bytes.push(b'\n');
    }
    std::fs::write(&path, bytes).expect("write invalid exact source");
    let mut limiter =
        RolloutMigrationRateLimiter::new(/*max_mib_per_second*/ None).expect("unlimited migration");

    assert!(prepare(&path, &mut limiter).await.is_err());
}

#[test]
fn catalog_carriers_survive_mixed_exact_and_count_only_rollback() {
    let thread_id = ThreadId::new();
    for tools in [catalog("replacement"), Vec::new()] {
        let retained = user("retained");
        let earlier_catalog = line(RolloutItem::DynamicToolsUpdated {
            thread_id,
            tools: catalog("earlier"),
        });
        let checkpoint = line(RolloutItem::Compacted(Box::new(catalog_checkpoint(
            thread_id,
            tools.clone(),
        ))));
        let expected_catalog = RolloutLine {
            timestamp: checkpoint.timestamp.clone(),
            ordinal: checkpoint.ordinal,
            item: RolloutItem::DynamicToolsUpdated { thread_id, tools },
        };
        let lines = vec![
            retained.clone(),
            user("count removed"),
            earlier_catalog.clone(),
            line(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent {
                    num_turns: 1,
                    materialized_turns: None,
                    rollback_start_index: None,
                },
            ))),
            user("exact removed"),
            checkpoint,
            exact(/*start*/ 4, /*num_turns*/ 1),
        ];
        let canonical = lines.iter().map(|line| line.item.clone()).collect();
        let positions = (0..lines.len()).collect();
        let records = lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| (index, Some(index), line))
            .collect();
        assert_eq!(
            serde_json::to_value(select(canonical, positions, records).expect("mixed replay"))
                .unwrap(),
            serde_json::to_value(vec![retained, earlier_catalog, expected_catalog]).unwrap(),
        );
    }
}

#[tokio::test]
async fn task_proofs_keep_physical_adjacency_before_mask_and_planner_replay() {
    use codex_history::ResponseItemEnvelope;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::AgentResponseFinalDelivery;
    use codex_protocol::protocol::AgentResponseObservation;
    use codex_protocol::protocol::AgentResponsePromotedTaskContext;
    use codex_protocol::protocol::new_user_agent_task_context_response_item_id;

    let home = tempfile::TempDir::new().expect("temporary directory");
    let path = home.path().join("physical-task-proof.jsonl");
    let legacy_gap = serde_json::json!({
        "timestamp": "2025-01-03T12:00:01Z",
        "ordinal": 19,
        "type": "event_msg",
        "payload": {
            "type": "exec_command_begin",
            "call_id": "compatibility-gap",
            "turn_id": "source-turn",
            "command": ["echo", "compatibility gap"],
            "cwd": std::env::temp_dir().to_string_lossy(),
            "parsed_cmd": [],
        },
    });
    assert!(
        codex_rollout::decode_canonical_rollout_line(legacy_gap.clone()).is_err(),
        "the plain cwd record must occupy only a physical compatibility position"
    );
    let compatibility_gap =
        super::super::line_parser::parse_legacy_rollout_value(legacy_gap.clone())
            .expect("normalize historical plain cwd")
            .expect("retain compatibility-only command");

    for tools in [catalog("physical-proof"), Vec::new()] {
        for remove_task in [false, true] {
            for (gap_after, compatibility) in [
                (None, false),
                (Some(0), false),
                (Some(1), false),
                (Some(0), true),
                (Some(1), true),
            ] {
                let owner = ThreadId::new();
                let task: ResponseItemEnvelope = ResponseItem::Message {
                    status: None,
                    encrypted_content: None,
                    id: Some(new_user_agent_task_context_response_item_id()),
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "<user_agent_task>review the change</user_agent_task>".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into();
                let metadata = line(RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                });
                let task_line = line(RolloutItem::ResponseItem(task.clone()));
                let observation = line(RolloutItem::AgentResponseObservation(
                    AgentResponseObservation {
                        observer_thread_id: owner,
                        target_thread_id: ThreadId::new(),
                        target_turn_id: Some("actual-turn".to_string()),
                        task_preview: None,
                        promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(
                            &task.item,
                        ),
                        target_messages: false,
                        queue_delivery: false,
                        message_wake_turn_id: None,
                        pending_commentary: false,
                        commentary_after_sequences: Vec::new(),
                        commentary_admissions: Vec::new(),
                        commentary_delivery: None,
                        reply_route_enabled: None,
                        reply_route_context_installed: false,
                        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
                        final_delivery: AgentResponseFinalDelivery::Wake,
                        final_delivery_response_item_id: None,
                        committed_delivery_response_item_ids: Vec::new(),
                        mailbox_final_subscription_message_id: None,
                        mailbox_final_subscription_suppressed_message_id: None,
                    },
                ));
                let checkpoint = RolloutLine {
                    timestamp: "2025-01-03T12:00:37Z".to_string(),
                    ordinal: Some(37),
                    item: RolloutItem::Compacted(Box::new(catalog_checkpoint(
                        owner,
                        tools.clone(),
                    ))),
                };
                let expected_catalog = RolloutLine {
                    timestamp: checkpoint.timestamp.clone(),
                    ordinal: checkpoint.ordinal,
                    item: RolloutItem::DynamicToolsUpdated {
                        thread_id: owner,
                        tools: tools.clone(),
                    },
                };
                let lines = [
                    metadata.clone(),
                    task_line.clone(),
                    observation.clone(),
                    user("removed exact input"),
                    checkpoint,
                    exact(if remove_task { 0 } else { 3 }, /*num_turns*/ 0),
                ];
                let mut bytes = Vec::new();
                for (index, record) in lines.iter().enumerate() {
                    serde_json::to_writer(&mut bytes, record).expect("serialize canonical record");
                    bytes.push(b'\n');
                    if gap_after == Some(index) {
                        if compatibility {
                            serde_json::to_writer(&mut bytes, &legacy_gap)
                                .expect("serialize compatibility-only gap");
                        } else {
                            bytes.extend_from_slice(b"{undecodable source record");
                        }
                        bytes.push(b'\n');
                    }
                }
                std::fs::write(&path, bytes).expect("write physical source history");
                let mut limiter =
                    RolloutMigrationRateLimiter::new(None).expect("unlimited migration");
                let selected = prepare(&path, &mut limiter)
                    .await
                    .expect("prepare source with exact cutoff")
                    .expect("detect exact marker");

                let trusted = gap_after.is_none();
                let mut expected = if remove_task && !trusted {
                    vec![observation]
                } else {
                    let mut expected_task = task_line;
                    if !trusted {
                        let RolloutItem::ResponseItem(envelope) = &mut expected_task.item else {
                            panic!("task envelope");
                        };
                        envelope.item.set_id(Some(ResponseItemId::with_suffix(
                            "msg",
                            format!("untrusted-{}", task.id().expect("task identity").as_str()),
                        )));
                    }
                    vec![metadata, expected_task, observation]
                };
                if compatibility && !remove_task {
                    expected.insert(
                        gap_after.expect("compatibility gap position") + 1,
                        compatibility_gap.clone(),
                    );
                }
                expected.push(expected_catalog);
                assert_eq!(
                    serde_json::to_value(&selected).expect("serialize selected history"),
                    serde_json::to_value(&expected).expect("serialize exact expected history"),
                    "physical gap {gap_after:?}, compatibility {compatibility}, cutoff includes task {remove_task}"
                );
                let selected_items = selected
                    .iter()
                    .map(|line| line.item.clone())
                    .collect::<Vec<_>>();
                assert_eq!(
                    codex_history::latest_dynamic_tools(&selected_items),
                    Some(tools.as_slice())
                );
            }
        }
    }
}
