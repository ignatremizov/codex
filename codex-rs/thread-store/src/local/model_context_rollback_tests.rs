use super::*;
use crate::local::rollout_lineage::RolloutLineageSegment;
use codex_protocol::protocol::ThreadRolledBackEvent;

#[tokio::test]
async fn exact_model_context_is_masked_before_segment_headers_are_removed() {
    for tools in [exact_rollback_catalog("replacement"), Vec::new()] {
        let home = TempDir::new().expect("temp dir");
        let parent_uuid = Uuid::from_u128(/*v*/ 9001);
        let child_uuid = Uuid::from_u128(/*v*/ 9002);
        let parent_id = ThreadId::from_string(&parent_uuid.to_string()).expect("parent");
        let child_id = ThreadId::from_string(&child_uuid.to_string()).expect("child");
        let marker = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(2),
        }));
        let parent_catalog = RolloutItem::DynamicToolsUpdated {
            thread_id: parent_id,
            tools: exact_rollback_catalog("foreign-parent"),
        };
        let child_catalog = RolloutItem::DynamicToolsUpdated {
            thread_id: child_id,
            tools: tools.clone(),
        };
        let child_checkpoint = RolloutItem::Compacted(Box::new(codex_history::CompactedItem {
            message: "removed child checkpoint".into(),
            resume_metadata: Some(codex_history::CompactionResumeMetadata {
                dynamic_tools: Some(Box::new(codex_history::DynamicToolsSnapshot {
                    thread_id: child_id,
                    tools: tools.clone(),
                })),
                multi_agent_version: Some(codex_protocol::protocol::MultiAgentVersion::V2),
                last_started_turn_id: Some("removed-child-turn".into()),
                turn_attribution: None,
                previous_turn_settings: None,
            }),
            ..Default::default()
        }));
        let parent_retained = user_message("parent retained");
        let parent_removed = user_message("parent removed after frozen boundary");
        let child_retained = user_message("child retained");
        let parent = write_ordinaled_paginated_rollout(
            home.path(),
            "2025-01-03T13-00-00",
            parent_uuid,
            [
                parent_retained.clone(),
                parent_removed.clone(),
                parent_catalog.clone(),
                marker.clone(),
            ],
        );
        let child = write_ordinaled_paginated_rollout(
            home.path(),
            "2025-01-03T14-00-00",
            child_uuid,
            [
                child_retained.clone(),
                user_message("child removed"),
                child_checkpoint,
                parent_catalog.clone(),
                marker,
            ],
        );
        let metadata = codex_rollout::read_session_meta_line(&child)
            .await
            .expect("child metadata");
        let parent_source = std::fs::read_to_string(&parent).expect("parent source");
        let frozen_length = parent_source
            .split_inclusive('\n')
            .take(3)
            .map(str::len)
            .sum::<usize>();
        for path in [&parent, &child] {
            let bytes = std::fs::read(path).expect("canonical bytes");
            std::fs::write(
                path.with_extension("jsonl.zst"),
                zstd::encode_all(bytes.as_slice(), /*level*/ 0).expect("compress fixture"),
            )
            .expect("compressed fixture");
            std::fs::remove_file(path).expect("compressed-only fixture");
        }
        let original_parent =
            std::fs::read(parent.with_extension("jsonl.zst")).expect("parent bytes");
        let original_child = std::fs::read(child.with_extension("jsonl.zst")).expect("child bytes");
        for frozen in [false, true] {
            let lineage = RolloutLineage {
                segments: vec![
                    RolloutLineageSegment {
                        thread_id: parent_id,
                        rollout_id: parent_id,
                        rollout_path: parent.clone(),
                        start_ordinal: 0,
                        end: frozen.then_some(HistoryPosition {
                            thread_id: parent_id,
                            end_ordinal_exclusive: 3,
                            end_byte_offset: u64::try_from(frozen_length).expect("length"),
                        }),
                    },
                    RolloutLineageSegment {
                        thread_id: child_id,
                        rollout_id: child_id,
                        rollout_path: child.clone(),
                        start_ordinal: 0,
                        end: None,
                    },
                ],
            };
            let actual =
                scan_model_context_from_lineage_blocking(&lineage, metadata.clone(), || false)
                    .expect("canonical context");
            let mut expected = vec![
                RolloutItem::SessionMeta(Box::new(metadata.clone())),
                parent_retained.clone(),
            ];
            if frozen {
                expected.push(parent_removed.clone());
            } else {
                expected.push(parent_catalog.clone());
            }
            expected.push(child_retained.clone());
            expected.push(child_catalog.clone());
            expected.push(parent_catalog.clone());
            assert_eq!(
                serde_json::to_value(&actual).expect("actual"),
                serde_json::to_value(&expected).expect("expected"),
            );
            assert_eq!(
                codex_history::latest_dynamic_tools(&actual),
                Some(tools.as_slice())
            );
        }
        assert!(!parent.exists());
        assert!(!child.exists());
        assert_eq!(
            std::fs::read(parent.with_extension("jsonl.zst")).unwrap(),
            original_parent
        );
        assert_eq!(
            std::fs::read(child.with_extension("jsonl.zst")).unwrap(),
            original_child
        );
    }
}

fn exact_rollback_catalog(name: &str) -> Vec<codex_protocol::dynamic_tools::DynamicToolSpec> {
    vec![codex_protocol::dynamic_tools::DynamicToolSpec::Function(
        codex_protocol::dynamic_tools::DynamicToolFunctionSpec {
            name: name.into(),
            description: format!("Resolve {name} resources"),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"resource": {"type": "string"}},
                "required": ["resource"],
                "additionalProperties": false,
            }),
            defer_loading: true,
        },
    )]
}
