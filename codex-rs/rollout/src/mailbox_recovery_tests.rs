use super::RolloutRecorder;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn metadata(id: ThreadId, history_mode: &str) -> Value {
    json!({
        "timestamp": "2025-01-03T12:00:01Z",
        "type": "session_meta",
        "payload": {
            "session_id": id,
            "id": id,
            "timestamp": "2025-01-03T12:00:01Z",
            "cwd": ".",
            "originator": "test",
            "cli_version": "test",
            "source": "cli",
            "model_provider": "test-provider",
            "history_mode": history_mode,
        }
    })
}

#[tokio::test]
async fn mailbox_reader_skips_valid_foreign_future_metadata_without_changing_resume_contract() {
    let home = tempfile::tempdir().unwrap();
    let receiver = ThreadId::new();
    let records = [
        metadata(receiver, "legacy"),
        metadata(ThreadId::new(), "future"),
        json!({
            "timestamp": "2025-01-03T12:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "preserve original context"}],
            }
        }),
    ];
    let raw = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    for (name, extension) in [("plain", "jsonl"), ("compressed", "jsonl.zst")] {
        let path = home.path().join(format!("{name}.{extension}"));
        let bytes = if extension == "jsonl.zst" {
            zstd::stream::encode_all(raw.as_bytes(), 3).unwrap()
        } else {
            raw.as_bytes().to_vec()
        };
        std::fs::write(&path, &bytes).unwrap();
        let (ordinary, owner, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        let (mailbox, mailbox_owner, ambiguous) =
            RolloutRecorder::load_rollout_items_for_mailbox_recovery(&path)
                .await
                .unwrap();
        assert_eq!((owner, errors), (Some(receiver), 1));
        assert_eq!((mailbox_owner, ambiguous), (Some(receiver), 0));
        assert_eq!(ordinary.len(), 2);
        assert_eq!(
            serde_json::to_value(mailbox).unwrap(),
            serde_json::to_value(ordinary).unwrap()
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn mailbox_reader_keeps_malformed_and_unclassifiable_records_as_errors() {
    let home = tempfile::tempdir().unwrap();
    let receiver = ThreadId::new();
    let mut malformed_metadata = metadata(ThreadId::new(), "future");
    malformed_metadata["payload"]["id"] = json!("not-a-thread-id");
    let mut invalid_mode = metadata(ThreadId::new(), "future");
    invalid_mode["payload"]["history_mode"] = json!(123);
    let invalid_records = [
        "{\"type\":\"response_item\",\"payload\":".to_string(),
        metadata(receiver, "future").to_string(),
        malformed_metadata.to_string(),
        invalid_mode.to_string(),
        json!({
            "timestamp": "2025-01-03T12:00:02Z",
            "type": "response_item",
            "payload": { "type": "message", "role": "user", "content": "invalid" },
        })
        .to_string(),
        json!({
            "timestamp": "2025-01-03T12:00:02Z",
            "type": "event_msg",
            "payload": { "type": "item_completed", "item": "invalid" },
        })
        .to_string(),
        json!({
            "timestamp": "2025-01-03T12:00:02Z",
            "type": "future_delivery",
            "payload": {},
        })
        .to_string(),
    ];
    for invalid in invalid_records {
        let path = home.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            format!("{}\n{invalid}\n", metadata(receiver, "legacy")),
        )
        .unwrap();
        let (ordinary, owner, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        let (mailbox, mailbox_owner, ambiguous) =
            RolloutRecorder::load_rollout_items_for_mailbox_recovery(&path)
                .await
                .unwrap();
        assert_eq!((owner, errors), (Some(receiver), 1), "{invalid}");
        assert_eq!((mailbox_owner, ambiguous), (Some(receiver), 1), "{invalid}");
        assert_eq!(
            serde_json::to_value(mailbox).unwrap(),
            serde_json::to_value(ordinary).unwrap()
        );
    }
}

#[tokio::test]
async fn mailbox_reader_cannot_ignore_unknown_mode_before_receiver_ownership_is_established() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("rollout.jsonl");
    std::fs::write(&path, format!("{}\n", metadata(ThreadId::new(), "future"))).unwrap();
    assert!(RolloutRecorder::load_rollout_items(&path).await.is_err());
    assert!(
        RolloutRecorder::load_rollout_items_for_mailbox_recovery(&path)
            .await
            .is_err()
    );
}
