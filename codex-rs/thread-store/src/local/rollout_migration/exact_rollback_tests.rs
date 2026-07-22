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

#[test]
fn zero_instruction_exact_rollback_removes_compaction_only_suffix() {
    let retained = user("retained");
    let lines = [
        retained.clone(),
        line(RolloutItem::Compacted(CompactedItem {
            message: "removed checkpoint".to_string(),
            ..Default::default()
        })),
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
        serde_json::to_value(vec![retained]).expect("serialize expected")
    );
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
