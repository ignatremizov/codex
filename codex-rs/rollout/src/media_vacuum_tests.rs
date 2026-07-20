use std::path::PathBuf;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::*;

fn policy() -> CompactedMediaVacuumPolicy {
    CompactedMediaVacuumPolicy {
        reopenable_image_omission: "reopen with view_image".to_string(),
        unavailable_image_omission: "image unavailable".to_string(),
    }
}

fn write_rollout(path: &Path, records: &[Value]) {
    let contents = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{contents}\n")).expect("write rollout");
}

fn read_rollout(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read rollout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse rollout record"))
        .collect()
}

#[test]
fn vacuum_reclaims_superseded_checkpoint_media_only() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let original_image = "data:image/png;base64,original";
    let compacted_image = format!("data:image/png;base64,{}", "a".repeat(/*n*/ 4_096));
    let tool_image = "data:image/png;base64,tool-output";
    let earlier_repair_suffix_image = "data:image/png;base64,earlier-repair-suffix";
    let suffix_image = "data:image/png;base64,suffix";
    write_rollout(
        path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_image", "image_url": original_image}]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.000Z",
                "type": "compacted",
                "payload": {
                    "message": "old",
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "<image name=\"[Image #1]\" path=\"/tmp/old.png\">"},
                            {"type": "input_image", "image_url": compacted_image},
                            {"type": "input_text", "text": "</image>"}
                        ]
                    }, {
                        "type": "compaction",
                        "encrypted_content": "opaque-secret"
                    }, {
                        "type": "function_call_output",
                        "call_id": "image-tool-call",
                        "output": [{"type": "input_image", "image_url": tool_image}]
                    }]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.500Z",
                "type": "compacted",
                "payload": {
                    "message": "earlier repair",
                    "replacement_history_media_sanitized_prefix_len": 0,
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_image", "image_url": earlier_repair_suffix_image}]
                    }]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "compacted",
                "payload": {
                    "message": "repair",
                    "replacement_history_media_sanitized_prefix_len": 1,
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_image", "image_url": suffix_image}]
                    }]
                }
            }),
        ],
    );

    let report = vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum should succeed");
    let records = read_rollout(path.as_path());

    assert_eq!(report.omitted_image_count, 2);
    assert_eq!(report.records_rewritten, 1);
    assert!(report.bytes_after < report.bytes_before);
    assert_eq!(
        records[0]["payload"]["content"][0]["image_url"],
        original_image
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][0]["content"][1]["text"],
        "reopen with view_image"
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][1]["encrypted_content"],
        "opaque-secret"
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][2]["output"][0]["text"],
        "image unavailable"
    );
    assert_eq!(
        records[2]["payload"]["replacement_history"][0]["content"][0]["image_url"],
        earlier_repair_suffix_image
    );
    assert_eq!(
        records[3]["payload"]["replacement_history"][0]["content"][0]["image_url"],
        suffix_image
    );
}

#[test]
fn vacuum_preserves_unrelated_record_bytes_and_line_endings() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let legacy = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"old","opaque":"preserve\\nthis","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}],"unknown":{"z":1,"a":2}}}"#;
    let protected = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"repair","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#;
    fs::write(
        path.as_path(),
        format!("{legacy}\r\n{protected}\n").as_bytes(),
    )
    .expect("write rollout");

    vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum should succeed");

    let rewritten = fs::read_to_string(path).expect("read vacuumed rollout");
    assert!(rewritten.contains(
        r#""opaque":"preserve\\nthis","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"image unavailable"}]}],"unknown":{"z":1,"a":2}}"#
    ));
    assert!(rewritten.ends_with(&format!("\r\n{protected}\n")));
}

#[test]
fn vacuum_without_repair_checkpoint_leaves_original_unchanged() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "old",
                "replacement_history": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_image", "image_url": "data:image/png;base64,old"}]
                }]
            }
        })],
    );
    let before = fs::read(path.as_path()).expect("read original rollout");

    vacuum_compacted_media(path.as_path(), &policy())
        .expect_err("vacuum must require a protected repair checkpoint");

    assert_eq!(fs::read(path).expect("read unchanged rollout"), before);
}

#[test]
fn vacuum_preserves_rejected_rollout_records() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let protected = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"repair","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#;
    fs::write(
        path.as_path(),
        format!("{protected}\n{{invalid rollout record\n"),
    )
    .expect("write rollout");
    let before = fs::read(path.as_path()).expect("read original rollout");

    vacuum_compacted_media(path.as_path(), &policy())
        .expect("vacuum should tolerate canonical rejected records");

    assert_eq!(fs::read(path).expect("read unchanged rollout"), before);
}

#[test]
fn vacuum_accepts_relative_path_and_rejects_pathless_image_wrapper() {
    let path = tempfile::Builder::new()
        .prefix("codex-relative-media-vacuum-")
        .suffix(".jsonl")
        .tempfile_in(".")
        .expect("relative rollout")
        .into_temp_path();
    let relative_path = PathBuf::from(path.file_name().expect("rollout file name"));
    write_rollout(
        relative_path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "old",
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "<image name=[Image #1]>"},
                            {"type": "input_image", "image_url": "data:image/png;base64,old"},
                            {"type": "input_text", "text": "</image>"}
                        ]
                    }]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.000Z",
                "type": "compacted",
                "payload": {
                    "message": "repair",
                    "replacement_history_media_sanitized_prefix_len": 0,
                    "replacement_history": []
                }
            }),
        ],
    );

    vacuum_compacted_media(relative_path.as_path(), &policy()).expect("vacuum relative rollout");

    assert_eq!(
        read_rollout(relative_path.as_path())[0]["payload"]["replacement_history"][0]["content"][1]
            ["text"],
        "image unavailable"
    );
}

#[test]
fn missing_canonical_rollout_recovers_the_newest_valid_vacuum_backup() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let protected = json!({
        "timestamp": "2026-01-01T00:00:00.000Z",
        "type": "compacted",
        "payload": {
            "message": "repair",
            "replacement_history_media_sanitized_prefix_len": 0,
            "replacement_history": []
        }
    });
    write_rollout(path.as_path(), &[protected]);
    let expected = fs::read(path.as_path()).expect("read canonical rollout");
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path.as_path(), backup_path.as_path()).expect("create vacuum backup");
    fs::remove_file(path.as_path()).expect("remove canonical rollout");

    recover_compacted_media_backup_if_needed(path.as_path()).expect("recover vacuum backup");

    assert_eq!(fs::read(path).expect("read recovered rollout"), expected);
    assert!(!backup_path.exists());
}

#[test]
fn missing_canonical_rollout_without_a_parent_directory_needs_no_recovery() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("missing-parent").join("rollout.jsonl");

    recover_compacted_media_backup_if_needed(path.as_path())
        .expect("a new rollout has no backup to recover");

    assert!(!path.exists());
}
