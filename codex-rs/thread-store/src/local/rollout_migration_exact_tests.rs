use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn exact_migration_uses_full_canonical_positions_and_preserves_media_timestamps() {
    let home = TempDir::new().expect("create Codex home");
    let thread_id = ThreadId::new();
    let media = rollout_response_item(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "retained image".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::File {
                    file_id: "opaque-retained-image".to_string(),
                },
                detail: None,
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let path = write_rollout(
        home.path(),
        thread_id,
        SessionSource::Cli,
        vec![media.clone()],
    );
    let mut lines = read_rollout(&path);
    let header = lines.remove(0);
    let mut retained_media = lines.remove(0);
    retained_media.timestamp = "2025-01-03T11:59:00Z".to_string();
    let guardian = codex_rollout::decode_rollout_line(json!({
        "timestamp": TIMESTAMP,
        "type": "event_msg",
        "payload": {
            "type": "guardian_assessment",
            "id": "retired-but-canonical",
            "status": "in_progress",
            "action": {
                "type": "network_access",
                "target": "example.com",
                "host": "example.com",
                "protocol": "https",
                "port": 443
            }
        }
    }))
    .expect("valid canonical retired event");
    let marker = RolloutLine {
        timestamp: TIMESTAMP.to_string(),
        ordinal: None,
        item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(4),
        })),
    };
    let mut copied_header = header.clone();
    let RolloutItem::SessionMeta(metadata) = &mut copied_header.item else {
        panic!("copied metadata");
    };
    metadata.meta.id = ThreadId::new();
    metadata.meta.session_id = metadata.meta.id.into();
    let source = vec![
        guardian,
        header, // Delayed authoritative metadata still occupies a canonical index.
        retained_media.clone(),
        copied_header, // Copied metadata is not removed until after exact coordinate mapping.
        RolloutLine {
            timestamp: TIMESTAMP.to_string(),
            ordinal: None,
            item: compacted(vec![input_response_message("user", "removed checkpoint")]),
        },
        marker,
    ];
    fs::write(&path, serialize_rollout(&source)).expect("write exact source");
    let store = indexed_store(home.path()).await;
    let report = store
        .migrate_rollouts(apply_options())
        .await
        .expect("migrate exact source");
    assert_eq!(report.outcomes[0].status, RolloutMigrationStatus::Migrated);
    let migrated = read_rollout(&path);
    assert_eq!(
        migrated
            .iter()
            .filter(|line| matches!(&line.item, RolloutItem::ResponseItem(_)))
            .map(|line| (
                line.timestamp.clone(),
                serde_json::to_value(&line.item).expect("item")
            ))
            .collect::<Vec<_>>(),
        vec![(
            retained_media.timestamp,
            serde_json::to_value(media).expect("media")
        )]
    );
    assert!(
        !migrated
            .iter()
            .any(|line| matches!(&line.item, RolloutItem::Compacted(_)))
    );
}

#[tokio::test]
async fn invalid_exact_mapping_preserves_source_projection_and_recovery_artifacts() {
    let home = TempDir::new().expect("create Codex home");
    let thread_id = ThreadId::new();
    let path = write_rollout(
        home.path(),
        thread_id,
        SessionSource::Cli,
        vec![user_message("SOURCE_SENTINEL")],
    );
    let mut legacy = read_rollout(&path);
    let store = indexed_store(home.path()).await;
    store
        .migrate_rollouts(apply_options())
        .await
        .expect("seed projection");
    let projection = thread_history::projection_state(&store, thread_id)
        .await
        .expect("read projection")
        .expect("seeded projection");
    legacy.push(RolloutLine {
        timestamp: TIMESTAMP.to_string(),
        ordinal: None,
        item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(u64::MAX),
        })),
    });
    let original = serialize_rollout(&legacy);
    fs::write(&path, &original).expect("restore invalid exact legacy source");
    let journal = migration_journal_path(home.path(), thread_id);
    write_migration_journal(&journal).await.expect("journal");
    let staged = staged_rollout_path(&path).expect("stage path");
    let rewritten = rewritten_staged_rollout_path(&staged).expect("rewritten stage path");
    fs::write(&staged, b"staged recovery bytes").expect("stage");
    fs::write(&rewritten, b"rewritten recovery bytes").expect("rewritten stage");
    let recovery =
        [&journal, &staged, &rewritten].map(|path| fs::read(path).expect("read recovery artifact"));

    let report = store
        .migrate_rollouts(apply_options())
        .await
        .expect("report mapping failure");
    assert_failed_with_reason(
        &report.outcomes[0],
        RolloutMigrationFailureReason::LegacyRolloutConversionFailed,
    );
    assert_eq!(
        fs::read(&path).expect("preserved source"),
        original.as_bytes()
    );
    assert_eq!(
        [&journal, &staged, &rewritten]
            .map(|path| fs::read(path).expect("read preserved recovery artifact")),
        recovery
    );
    let after = thread_history::projection_state(&store, thread_id)
        .await
        .expect("read preserved projection")
        .expect("projection not deleted");
    assert_eq!(
        (after.next_byte_offset, after.next_ordinal),
        (projection.next_byte_offset, projection.next_ordinal)
    );
}
