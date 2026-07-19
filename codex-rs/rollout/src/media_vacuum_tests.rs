use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::*;

fn policy() -> CompactedMediaVacuumPolicy {
    CompactedMediaVacuumPolicy {
        reopenable_image_omission: "reopen with view_image".to_string(),
        unavailable_image_omission: "image unavailable".to_string(),
        mixed_image_omission: "some images unavailable".to_string(),
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

fn compress_rollout(path: &Path) -> PathBuf {
    let compressed_path = crate::compression::compressed_rollout_path(path);
    let input = fs::File::open(path).expect("open rollout for compression");
    let output = fs::File::create(&compressed_path).expect("create compressed rollout");
    let mut encoder =
        zstd::stream::write::Encoder::new(output, /*level*/ 3).expect("create zstd encoder");
    std::io::copy(&mut std::io::BufReader::new(input), &mut encoder).expect("compress rollout");
    encoder.finish().expect("finish compressed rollout");
    fs::remove_file(path).expect("remove materialized rollout");
    compressed_path
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
                            {"type": "input_text", "text": "</image>"},
                            {"type": "input_text", "text": "image unavailable"}
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
                    "replacement_history_media_sanitized_prefix_len": 1,
                    "replacement_history": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "media-free prefix"}]
                    }, {
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
                    "replacement_history_media_sanitized_prefix_len": 0,
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_image", "image_url": suffix_image}]
                    }]
                }
            }),
        ],
    );
    let original_records = read_rollout(path.as_path());

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
        ""
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][0]["content"][3]["text"],
        ""
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][1]["encrypted_content"],
        "opaque-secret"
    );
    assert_eq!(
        records[1]["payload"]["replacement_history"][2]["output"][0]["text"],
        "some images unavailable"
    );
    assert_eq!(records[2], original_records[2]);
    assert_eq!(records[3], original_records[3]);
}

#[cfg(not(unix))]
#[test]
fn vacuum_retains_recovery_backup_without_a_directory_sync_barrier() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "old",
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_image",
                            "image_url": "data:image/png;base64,legacy"
                        }]
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
    let original = fs::read(path.as_path()).expect("read original rollout");

    vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum rollout");

    let backups = fs::read_dir(temp_dir.path())
        .expect("read rollout directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| compacted_media_backup_id(name, "rollout.jsonl").is_some())
        })
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(backups[0].as_path()).expect("read retained recovery backup"),
        original
    );
}

#[test]
fn vacuum_preserves_unrelated_record_bytes_and_line_endings() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let pre_policy_compacted = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"old","opaque":"preserve\\nthis","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}],"unknown":{"z":1,"a":2}}}"#;
    let protected = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"repair","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#;
    fs::write(
        path.as_path(),
        format!("{pre_policy_compacted}\r\n{protected}\n").as_bytes(),
    )
    .expect("write rollout");
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::OpenOptions::new()
        .write(true)
        .open(path.as_path())
        .expect("open rollout metadata")
        .set_times(fs::FileTimes::new().set_modified(modified))
        .expect("set rollout modification time");

    vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum should succeed");

    let rewritten = fs::read_to_string(path).expect("read vacuumed rollout");
    assert!(rewritten.contains(
        r#""opaque":"preserve\\nthis","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"image unavailable"}]}],"unknown":{"z":1,"a":2}}"#
    ));
    assert!(rewritten.ends_with(&format!("\r\n{protected}\n")));
    assert_eq!(
        fs::metadata(temp_dir.path().join("rollout.jsonl"))
            .expect("vacuumed rollout metadata")
            .modified()
            .expect("vacuumed rollout modification time"),
        modified
    );
}

#[test]
fn vacuum_repairs_checkpointless_rollout_and_preserves_transcript_media() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let image_url = "data:image/png;base64,old";
    let custom_tool_image_url = "data:image/png;base64,custom-tool";
    write_rollout(
        path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_image", "image_url": image_url}]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.000Z",
                "type": "compacted",
                "payload": {
                    "message": "mentions \"replacement_history_media_sanitized_prefix_len\"",
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_image", "image_url": image_url}]
                    }]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "compacted",
                "payload": {
                    "message": "custom tool output",
                    "replacement_history": [{
                        "type": "custom_tool_call_output",
                        "call_id": "custom-image-call",
                        "output": [{
                            "type": "input_image",
                            "image_url": custom_tool_image_url
                        }]
                    }]
                }
            }),
        ],
    );
    let original_records = read_rollout(path.as_path());
    let bytes_before = fs::metadata(path.as_path())
        .expect("original rollout metadata")
        .len();

    let report = vacuum_compacted_media(path.as_path(), &policy())
        .expect("checkpointless rollout should be repaired");
    let bytes_after = fs::metadata(path.as_path())
        .expect("vacuumed rollout metadata")
        .len();
    let records = read_rollout(path.as_path());

    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before,
            bytes_after,
            records_rewritten: 2,
            omitted_image_count: 2,
            omitted_inline_media_bytes: u64::try_from(
                image_url.len() + custom_tool_image_url.len()
            )
            .expect("image URL length should fit in u64"),
        }
    );
    assert_eq!(records[0], original_records[0]);
    assert_eq!(
        records[1]["payload"]["replacement_history"][0]["content"][0],
        json!({"type": "input_text", "text": "image unavailable"})
    );
    assert_eq!(
        records[2]["payload"]["replacement_history"][0]["output"][0],
        json!({"type": "input_text", "text": "image unavailable"})
    );
    let vacuumed_bytes = fs::read(path.as_path()).expect("read vacuumed rollout");
    assert_eq!(
        vacuum_compacted_media(path.as_path(), &policy()).expect("repeat vacuum should be a no-op"),
        CompactedMediaVacuumReport {
            bytes_before: bytes_after,
            bytes_after,
            ..Default::default()
        }
    );
    assert_eq!(
        fs::read(path.as_path()).expect("read repeated vacuum rollout"),
        vacuumed_bytes
    );
    assert!(
        fs::read_dir(temp_dir.path())
            .expect("read rollout directory")
            .filter_map(Result::ok)
            .all(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| compacted_media_backup_id(name, "rollout.jsonl").is_none())
            })
    );
}

#[test]
fn vacuum_preserves_rejected_rollout_records_during_rewrite() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let pre_policy_compacted = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"old","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}]}}"#;
    let rejected_suffix = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,rejected"}]}]}}
{invalid rollout record
"#;
    fs::write(
        path.as_path(),
        format!("{pre_policy_compacted}\r\n{rejected_suffix}"),
    )
    .expect("write rollout");

    vacuum_compacted_media(path.as_path(), &policy())
        .expect("vacuum should preserve rejected records");

    let rewritten = fs::read(path).expect("read rewritten rollout");
    assert!(rewritten.ends_with(rejected_suffix.as_bytes()));
    assert!(!rewritten.starts_with(pre_policy_compacted.as_bytes()));
}

#[test]
fn schema_invalid_or_duplicate_marker_records_fail_closed() {
    let temp_dir = TempDir::new().expect("temp dir");
    let pre_policy_compacted = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"old","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}]}}"#;
    let protected = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"protected","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#;
    let invalid_markers = [
        r#"{"type":"compacted","payload":{"message":"missing timestamp","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"duplicate marker","replacement_history_media_sanitized_prefix_len":0,"replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"invalid role","replacement_history_media_sanitized_prefix_len":1,"replacement_history":[{"type":"message","role":7,"content":[]}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"invalid content","replacement_history_media_sanitized_prefix_len":1,"replacement_history":[{"type":"message","role":"user","content":{}}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"invalid image","replacement_history_media_sanitized_prefix_len":1,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":7}]}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":7,"payload":{"replacement_history_media_sanitized_prefix_le\u006e":0}}"#,
        r#"[{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"replacement_history_media_sanitized_prefix_le\u006e":0}}]"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":[{"replacement_history_media_sanitized_prefix_le\u006e":0}]}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"replacement_history_media_sanitized_prefix_le\u006e"}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"replacement_history_media_sanitized_prefix_le\u006e":0,"replacement_history":[]},BROKEN}"#,
    ];

    for (index, invalid_marker) in invalid_markers.into_iter().enumerate() {
        let path = temp_dir.path().join(format!("rollout-{index}.jsonl"));
        fs::write(
            path.as_path(),
            format!("{pre_policy_compacted}\n{protected}\n{invalid_marker}\n"),
        )
        .expect("write rollout");
        let before = fs::read(path.as_path()).expect("read original rollout");

        vacuum_compacted_media(path.as_path(), &policy())
            .expect_err("one protected checkpoint must not mask an invalid marker");

        assert_eq!(fs::read(path).expect("read unchanged rollout"), before);
    }
}

#[test]
fn invalid_marker_without_rewritable_media_fails_before_no_op() {
    let temp_dir = TempDir::new().expect("temp dir");
    for use_compressed in [false, true] {
        let case_dir = temp_dir
            .path()
            .join(format!("invalid-no-op-{use_compressed}"));
        fs::create_dir(&case_dir).expect("create invalid marker case");
        let path = case_dir.join("rollout.jsonl");
        write_rollout(
            path.as_path(),
            &[json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "invalid marker without media",
                    "replacement_history_media_sanitized_prefix_len": null,
                    "replacement_history": []
                }
            })],
        );
        let physical_path = if use_compressed {
            compress_rollout(path.as_path())
        } else {
            path.clone()
        };
        let before = fs::read(physical_path.as_path()).expect("read selected rollout");

        let error = vacuum_compacted_media(path.as_path(), &policy())
            .expect_err("invalid marker must fail before no-op");

        assert_eq!(
            error.to_string(),
            "refusing compacted-media vacuum with an invalid sanitized replacement-history checkpoint"
        );
        assert_eq!(
            fs::read(physical_path.as_path()).expect("read unchanged selected rollout"),
            before
        );
        assert_eq!(path.exists(), !use_compressed);
    }
}

#[test]
fn false_media_free_certification_fails_closed() {
    let temp_dir = TempDir::new().expect("temp dir");
    let protected_marker = json!({
        "timestamp": "2026-01-01T00:00:00.500Z",
        "type": "compacted",
        "payload": {
            "message": "protected",
            "replacement_history_media_sanitized_prefix_len": 0,
            "replacement_history": []
        }
    });
    let invalid_markers = [
        json!({
            "timestamp": "2026-01-01T00:00:01.000Z",
            "type": "compacted",
            "payload": {
                "message": "prefix still contains media",
                "replacement_history_media_sanitized_prefix_len": 1,
                "replacement_history": [{
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": format!("data:image/png;base64,{}", "a".repeat(/*n*/ 4_096))
                    }]
                }]
            }
        }),
        json!({
            "timestamp": "2026-01-01T00:00:01.000Z",
            "type": "compacted",
            "payload": {
                "message": "prefix exceeds history",
                "replacement_history_media_sanitized_prefix_len": 2,
                "replacement_history": [{
                    "type": "message",
                    "role": "assistant",
                    "content": []
                }]
            }
        }),
    ];

    for (index, invalid_marker) in invalid_markers.into_iter().enumerate() {
        let path = temp_dir.path().join(format!("rollout-{index}.jsonl"));
        write_rollout(
            path.as_path(),
            &[
                json!({
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "type": "compacted",
                    "payload": {
                        "message": "old",
                        "replacement_history": [{
                            "type": "message",
                            "role": "user",
                            "content": [{
                                "type": "input_image",
                                "image_url": "data:image/png;base64,old"
                            }]
                        }]
                    }
                }),
                protected_marker.clone(),
                invalid_marker,
            ],
        );
        let before = fs::read(path.as_path()).expect("read original rollout");

        vacuum_compacted_media(path.as_path(), &policy())
            .expect_err("one protected checkpoint must not mask false certification");

        assert_eq!(fs::read(path).expect("read unchanged rollout"), before);
    }
}

#[test]
fn vacuum_preserves_schema_invalid_compacted_records() {
    let temp_dir = TempDir::new().expect("temp dir");
    let protected = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"compacted","payload":{"message":"repair","replacement_history_media_sanitized_prefix_len":0,"replacement_history":[]}}"#;
    let invalid_records = [
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"invalid role","replacement_history":[{"type":"message","role":7,"content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"invalid content","replacement_history":[{"type":"message","role":"user","content":{}}]}}"#,
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"invalid image","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":7}]}]}}"#,
    ];

    for (index, invalid_record) in invalid_records.into_iter().enumerate() {
        let path = temp_dir.path().join(format!("rollout-{index}.jsonl"));
        fs::write(path.as_path(), format!("{invalid_record}\n{protected}\n"))
            .expect("write rollout");
        let before = fs::read(path.as_path()).expect("read original rollout");

        let report = vacuum_compacted_media(path.as_path(), &policy())
            .expect("valid marker authorizes vacuum");

        assert_eq!(report.records_rewritten, 0);
        assert_eq!(fs::read(path).expect("read unchanged rollout"), before);
    }
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
fn vacuum_materializes_logical_and_physical_compressed_rollout_paths() {
    for use_physical_compressed_path in [false, true] {
        let prefix =
            format!("codex-relative-compressed-media-vacuum-{use_physical_compressed_path}-");
        let path = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(".jsonl")
            .tempfile_in(".")
            .expect("relative compressed rollout")
            .into_temp_path();
        let path = PathBuf::from(path.file_name().expect("compressed rollout file name"));
        let image_url = format!("data:image/png;base64,{}", "a".repeat(/*n*/ 4_096));
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
                        "content": [{
                            "type": "input_image",
                            "image_url": image_url.clone()
                        }]
                    }]
                }
            })],
        );
        let compressed_path = compress_rollout(path.as_path());
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100);
        fs::OpenOptions::new()
            .write(true)
            .open(compressed_path.as_path())
            .expect("open compressed rollout metadata")
            .set_times(fs::FileTimes::new().set_modified(modified))
            .expect("set compressed rollout modification time");
        let compressed_bytes = fs::metadata(compressed_path.as_path())
            .expect("compressed rollout metadata")
            .len();
        let requested_path = if use_physical_compressed_path {
            compressed_path.as_path()
        } else {
            path.as_path()
        };

        let report =
            vacuum_compacted_media(requested_path, &policy()).expect("vacuum compressed rollout");
        let bytes_after = fs::metadata(path.as_path())
            .expect("vacuumed rollout metadata")
            .len();

        assert_eq!(
            report,
            CompactedMediaVacuumReport {
                bytes_before: compressed_bytes,
                bytes_after,
                records_rewritten: 1,
                omitted_image_count: 1,
                omitted_inline_media_bytes: u64::try_from(image_url.len())
                    .expect("image URL length should fit in u64"),
            }
        );
        assert!(path.exists());
        assert_eq!(
            fs::metadata(path.as_path())
                .expect("vacuumed rollout metadata")
                .modified()
                .expect("vacuumed rollout modification time"),
            modified
        );
        #[cfg(unix)]
        assert!(!compressed_path.exists());
        #[cfg(not(unix))]
        assert!(compressed_path.exists());
        assert_eq!(
            read_rollout(path.as_path())[0]["payload"]["replacement_history"][0]["content"][0]["text"],
            "image unavailable"
        );
        #[cfg(not(unix))]
        {
            assert_eq!(
                vacuum_compacted_media(path.as_path(), &policy())
                    .expect("later vacuum cleans retained recovery representations"),
                CompactedMediaVacuumReport {
                    bytes_before: bytes_after,
                    bytes_after,
                    ..Default::default()
                }
            );
            assert!(!compressed_path.exists());
        }
    }
}

#[test]
fn vacuum_preserves_compressed_mtime_when_post_materialization_cleanup_fails() {
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
                    "content": [{
                        "type": "input_image",
                        "image_url": "data:image/png;base64,old"
                    }]
                }]
            }
        })],
    );
    let compressed_path = compress_rollout(path.as_path());
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_200);
    fs::OpenOptions::new()
        .write(true)
        .open(compressed_path.as_path())
        .expect("open compressed rollout metadata")
        .set_times(fs::FileTimes::new().set_modified(modified))
        .expect("set compressed rollout modification time");
    fs::create_dir(
        temp_dir
            .path()
            .join(".rollout.jsonl.media-vacuum-blocked.tmp"),
    )
    .expect("create obstructing vacuum artifact");

    vacuum_compacted_media(path.as_path(), &policy())
        .expect_err("artifact cleanup should fail after compressed materialization");

    assert!(path.exists());
    assert_eq!(
        fs::metadata(path.as_path())
            .expect("materialized rollout metadata")
            .modified()
            .expect("materialized rollout modification time"),
        modified
    );
}

#[test]
fn checkpointless_no_op_preserves_compressed_rollout_representation() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "unprotected",
                "replacement_history": []
            }
        })],
    );
    let compressed_path = compress_rollout(path.as_path());
    let compressed_bytes = fs::metadata(compressed_path.as_path())
        .expect("compressed rollout metadata")
        .len();

    let report =
        vacuum_compacted_media(path.as_path(), &policy()).expect("checkpointless no-op succeeds");

    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before: compressed_bytes,
            bytes_after: compressed_bytes,
            ..Default::default()
        }
    );
    assert!(!path.exists());
    assert!(compressed_path.exists());
}

#[test]
fn no_op_plain_rollout_removes_obsolete_compressed_sibling() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "stale compressed media",
                "replacement_history": [{
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": "data:image/png;base64,stale"
                    }]
                }]
            }
        })],
    );
    let compressed_path = compress_rollout(path.as_path());
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:01.000Z",
            "type": "compacted",
            "payload": {
                "message": "canonical plain",
                "replacement_history": []
            }
        })],
    );
    let plain_bytes = fs::metadata(path.as_path())
        .expect("plain rollout metadata")
        .len();

    let report =
        vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum canonical plain rollout");

    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before: plain_bytes,
            bytes_after: plain_bytes,
            ..Default::default()
        }
    );
    assert!(path.exists());
    assert!(!compressed_path.exists());
}

#[test]
fn invalid_plain_rollout_does_not_remove_valid_compressed_sibling() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "valid compressed rollout",
                "replacement_history": []
            }
        })],
    );
    let compressed_path = compress_rollout(path.as_path());
    let compressed = fs::read(compressed_path.as_path()).expect("read compressed rollout");
    fs::write(path.as_path(), b" \n").expect("write invalid canonical plain rollout");

    let error = vacuum_compacted_media(path.as_path(), &policy())
        .expect_err("invalid plain rollout must not displace compressed sibling");

    assert_eq!(
        error.to_string(),
        "refusing to remove a compressed rollout sibling without a valid canonical plain record"
    );
    assert_eq!(
        fs::read(compressed_path.as_path()).expect("read preserved compressed rollout"),
        compressed
    );
    assert_eq!(
        fs::read(path).expect("read preserved invalid plain rollout"),
        b" \n"
    );
}

#[test]
fn no_op_vacuum_removes_retained_recovery_backup() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "protected",
                "replacement_history_media_sanitized_prefix_len": 0,
                "replacement_history": []
            }
        })],
    );
    let canonical = fs::read(path.as_path()).expect("read canonical rollout");
    let backup_source = temp_dir.path().join("backup-source.jsonl");
    fs::write(backup_source.as_path(), b"stale pre-vacuum media").expect("write backup source");
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(backup_source.as_path(), backup_path.as_path()).expect("create retained backup");
    fs::remove_file(backup_source).expect("remove backup source name");
    let temporary_path = path.with_file_name(".rollout.jsonl.media-vacuum-interrupted.tmp");
    fs::write(temporary_path.as_path(), b"interrupted vacuum output")
        .expect("write retained temporary file");

    let report =
        vacuum_compacted_media(path.as_path(), &policy()).expect("vacuum canonical rollout");

    let canonical_bytes =
        u64::try_from(canonical.len()).expect("canonical rollout length should fit in u64");
    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before: canonical_bytes,
            bytes_after: canonical_bytes,
            ..Default::default()
        }
    );
    assert_eq!(fs::read(path).expect("read unchanged rollout"), canonical);
    assert!(!backup_path.exists());
    assert!(!temporary_path.exists());
}

#[test]
fn invalid_no_op_canonical_rollout_preserves_recovery_artifacts() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "protected",
                "replacement_history_media_sanitized_prefix_len": 0,
                "replacement_history": []
            }
        })],
    );
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path.as_path(), backup_path.as_path()).expect("create retained backup");
    let temporary_path = path.with_file_name(".rollout.jsonl.media-vacuum-interrupted.tmp");
    fs::write(temporary_path.as_path(), b"interrupted vacuum output")
        .expect("write retained temporary file");
    let invalid_path = path.with_file_name("invalid-rollout.jsonl");
    fs::write(invalid_path.as_path(), b" \n").expect("write invalid replacement");
    fs::rename(invalid_path, path.as_path()).expect("replace canonical with invalid content");

    let report =
        vacuum_compacted_media(path.as_path(), &policy()).expect("inspect invalid canonical");

    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before: 2,
            bytes_after: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        fs::read(path).expect("read invalid canonical rollout"),
        b" \n"
    );
    assert!(backup_path.exists());
    assert!(temporary_path.exists());
}

#[test]
fn opaque_nested_image_preserves_compressed_no_op_representation() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let opaque_item = json!({
        "type": "tool_search_output",
        "call_id": null,
        "status": "completed",
        "execution": "server",
        "tools": [{
            "type": "input_image",
            "image_url": "data:image/png;base64,opaque"
        }]
    });
    write_rollout(
        path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "opaque nested image",
                    "replacement_history": [opaque_item]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.000Z",
                "type": "compacted",
                "payload": {
                    "message": "protected opaque nested image",
                    "replacement_history_media_sanitized_prefix_len": 1,
                    "replacement_history": [opaque_item]
                }
            }),
        ],
    );
    let compressed_path = compress_rollout(path.as_path());
    let compressed_bytes = fs::metadata(compressed_path.as_path())
        .expect("compressed rollout metadata")
        .len();

    let report =
        vacuum_compacted_media(path.as_path(), &policy()).expect("opaque media is not rewritable");

    assert_eq!(
        report,
        CompactedMediaVacuumReport {
            bytes_before: compressed_bytes,
            bytes_after: compressed_bytes,
            ..Default::default()
        }
    );
    assert!(!path.exists());
    assert!(compressed_path.exists());
}

#[test]
fn invalid_marker_preserves_compressed_rollout_representation() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[
            json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "legacy",
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_image",
                            "image_url": "data:image/png;base64,old"
                        }]
                    }]
                }
            }),
            json!({
                "timestamp": "2026-01-01T00:00:01.000Z",
                "type": "compacted",
                "payload": {
                    "message": "invalid marker",
                    "replacement_history_media_sanitized_prefix_len": 1,
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_image",
                            "image_url": "data:image/png;base64,still-present"
                        }]
                    }]
                }
            }),
        ],
    );
    let compressed_path = compress_rollout(path.as_path());

    let error = vacuum_compacted_media(path.as_path(), &policy())
        .expect_err("invalid marker should fail preflight");

    assert_eq!(
        error.to_string(),
        "refusing compacted-media vacuum with an invalid sanitized replacement-history checkpoint"
    );
    assert!(!path.exists());
    assert!(compressed_path.exists());
}

#[test]
fn no_op_vacuum_preserves_compressed_rollout_representation() {
    let temp_dir = TempDir::new().expect("temp dir");
    for use_physical_compressed_path in [false, true] {
        let case_dir = temp_dir
            .path()
            .join(format!("no-op-compressed-{use_physical_compressed_path}"));
        fs::create_dir(&case_dir).expect("create compressed rollout case");
        let path = case_dir.join("rollout.jsonl");
        write_rollout(
            path.as_path(),
            &[json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "type": "compacted",
                "payload": {
                    "message": "protected",
                    "replacement_history_media_sanitized_prefix_len": 0,
                    "replacement_history": []
                }
            })],
        );
        let compressed_path = compress_rollout(path.as_path());
        let compressed_bytes = fs::metadata(compressed_path.as_path())
            .expect("compressed rollout metadata")
            .len();
        let requested_path = if use_physical_compressed_path {
            compressed_path.as_path()
        } else {
            path.as_path()
        };

        let report =
            vacuum_compacted_media(requested_path, &policy()).expect("no-op vacuum succeeds");

        assert_eq!(
            report,
            CompactedMediaVacuumReport {
                bytes_before: compressed_bytes,
                bytes_after: compressed_bytes,
                ..Default::default()
            }
        );
        assert!(!path.exists());
        assert!(compressed_path.exists());
    }
}

#[test]
fn missing_canonical_rollout_recovers_checkpointless_vacuum_backup() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let checkpointless = json!({
        "timestamp": "2026-01-01T00:00:00.000Z",
        "type": "compacted",
        "payload": {
            "message": "legacy",
            "replacement_history": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": "data:image/png;base64,old"
                }]
            }]
        }
    });
    write_rollout(path.as_path(), &[checkpointless]);
    let expected = fs::read(path.as_path()).expect("read canonical rollout");
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path.as_path(), backup_path.as_path()).expect("create vacuum backup");
    fs::remove_file(path.as_path()).expect("remove canonical rollout");

    recover_compacted_media_backup_if_needed(path.as_path()).expect("recover vacuum backup");

    assert_eq!(fs::read(path).expect("read recovered rollout"), expected);
    #[cfg(unix)]
    assert!(!backup_path.exists());
    #[cfg(not(unix))]
    assert_eq!(
        fs::read(backup_path).expect("read retained recovery backup"),
        expected
    );
}

#[test]
fn missing_canonical_rollout_rejects_marker_invalid_vacuum_backup() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    let checkpointless = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"compacted","payload":{"message":"old","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,old"}]}]}}"#;
    let invalid_marker = r#"{"timestamp":"2026-01-01T00:00:01Z","type":7,"payload":{"replacement_history_media_sanitized_prefix_le\u006e":0}}"#;
    fs::write(
        path.as_path(),
        format!("{checkpointless}\n{invalid_marker}\n"),
    )
    .expect("write marker-invalid rollout");
    let expected = fs::read(path.as_path()).expect("read canonical rollout");
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path.as_path(), backup_path.as_path()).expect("create vacuum backup");
    fs::remove_file(path.as_path()).expect("remove canonical rollout");

    recover_compacted_media_backup_if_needed(path.as_path()).expect("inspect vacuum backup");

    assert!(!path.exists());
    assert_eq!(
        fs::read(backup_path).expect("read rejected recovery backup"),
        expected
    );
}

#[tokio::test]
async fn physical_compressed_path_recovers_plain_vacuum_backup() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("rollout.jsonl");
    write_rollout(
        path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "repair",
                "replacement_history_media_sanitized_prefix_len": 0,
                "replacement_history": []
            }
        })],
    );
    let expected = fs::read_to_string(path.as_path()).expect("read canonical rollout");
    let backup_path = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path.as_path(), backup_path.as_path()).expect("create vacuum backup");
    fs::remove_file(path.as_path()).expect("remove canonical rollout");
    let compressed_path = crate::compression::compressed_rollout_path(path.as_path());

    let mut reader = crate::compression::open_rollout_line_reader(compressed_path.as_path())
        .await
        .expect("recover through physical compressed path");

    assert_eq!(
        reader.next_line().await.expect("read recovered rollout"),
        Some(expected.trim_end().to_string())
    );
    assert!(path.exists());
    #[cfg(unix)]
    assert!(!backup_path.exists());
    #[cfg(not(unix))]
    fs::remove_file(backup_path).expect("clean retained recovery backup");
}

#[test]
fn relative_missing_canonical_rollout_recovers_vacuum_backup() {
    let path = tempfile::Builder::new()
        .prefix("codex-relative-media-recovery-")
        .suffix(".jsonl")
        .tempfile_in(".")
        .expect("relative rollout")
        .into_temp_path();
    let relative_path = PathBuf::from(path.file_name().expect("rollout file name"));
    write_rollout(
        relative_path.as_path(),
        &[json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "type": "compacted",
            "payload": {
                "message": "repair",
                "replacement_history_media_sanitized_prefix_len": 0,
                "replacement_history": []
            }
        })],
    );
    let expected = fs::read(relative_path.as_path()).expect("read canonical rollout");
    let backup_path = relative_path.with_file_name(format!(
        ".{}.pre-media-vacuum-{}.bak",
        relative_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 rollout file name"),
        Uuid::now_v7()
    ));
    fs::hard_link(relative_path.as_path(), backup_path.as_path()).expect("create vacuum backup");
    fs::remove_file(relative_path.as_path()).expect("remove canonical rollout");

    recover_compacted_media_backup_if_needed(relative_path.as_path())
        .expect("recover relative vacuum backup");

    assert_eq!(
        fs::read(relative_path.as_path()).expect("read recovered rollout"),
        expected
    );
    #[cfg(unix)]
    assert!(!backup_path.exists());
    #[cfg(not(unix))]
    {
        assert_eq!(
            fs::read(backup_path.as_path()).expect("read retained recovery backup"),
            expected
        );
        fs::remove_file(backup_path).expect("clean retained recovery backup");
    }
}

#[test]
fn missing_canonical_rollout_without_a_parent_directory_needs_no_recovery() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("missing-parent").join("rollout.jsonl");

    recover_compacted_media_backup_if_needed(path.as_path())
        .expect("a new rollout has no backup to recover");

    assert!(!path.exists());
}
