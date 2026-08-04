use super::*;
use crate::local::rollout_lineage::RolloutLineageSegment;
use codex_protocol::protocol::ThreadRolledBackEvent;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn exact_model_context_is_masked_before_segment_headers_are_removed() {
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
    for frozen in [false, true] {
        let lineage = RolloutLineage {
            segments: vec![
                RolloutLineageSegment {
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
                    rollout_id: child_id,
                    rollout_path: child.clone(),
                    start_ordinal: 0,
                    end: None,
                },
            ],
        };
        let actual = scan_model_context_from_lineage_blocking(&lineage, metadata.clone())
            .expect("canonical context");
        let mut expected = vec![
            RolloutItem::SessionMeta(metadata.clone()),
            parent_retained.clone(),
        ];
        if frozen {
            expected.push(parent_removed.clone());
        }
        expected.push(child_retained.clone());
        assert_eq!(
            serde_json::to_value(actual).expect("actual"),
            serde_json::to_value(expected).expect("expected"),
        );
    }
    assert!(!parent.exists());
    assert!(!child.exists());
}
