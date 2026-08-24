use std::fs;
use std::fs::OpenOptions;
use std::io::Write;

use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_rollout::CompactedItem;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::super::LoadForkSourceByRolloutPathParams;
use super::super::LocalThreadStore;
use super::super::ThreadStoreError;
use super::super::rollout_lineage::OpenedRolloutLineage;
use super::super::rollout_lineage::OpenedRolloutLineageSegment;
use super::super::rollout_lineage::RolloutLineage;
use super::super::rollout_lineage::RolloutLineageSegment;
use super::super::test_support::test_config;
use super::load;
use super::load_complete_lineage;
use super::load_opened_source;
use super::load_segment_from_snapshot;

#[tokio::test]
async fn complete_lineage_rejects_ordinal_gaps() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{thread_id}.jsonl"));
    let session_meta = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            history_mode: ThreadHistoryMode::Paginated,
            ..SessionMeta::default()
        },
        git: None,
    };
    let lines = [
        rollout_line(
            /*ordinal*/ 0,
            RolloutItem::SessionMeta(session_meta.clone()),
        ),
        rollout_line(
            /*ordinal*/ 1,
            RolloutItem::EventMsg(EventMsg::ShutdownComplete),
        ),
        rollout_line(
            /*ordinal*/ 3,
            RolloutItem::EventMsg(EventMsg::ShutdownComplete),
        ),
    ];
    fs::write(path.as_path(), format!("{}\n", lines.join("\n"))).expect("write rollout");
    let lineage = RolloutLineage {
        segments: vec![RolloutLineageSegment {
            rollout_id: thread_id,
            rollout_path: path,
            start_ordinal: 1,
            end: None,
        }],
    };

    let err = load_lineage(lineage, session_meta)
        .await
        .expect_err("ordinal gap should fail");

    assert!(
        err.to_string()
            .contains("expected ordinal 2, found ordinal 3"),
        "{err}"
    );
}

#[tokio::test]
async fn complete_lineage_applies_exact_rollbacks_within_each_segment() {
    let home = TempDir::new().expect("temp dir");
    let root_id = ThreadId::default();
    let child_id = ThreadId::default();
    let root_path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{root_id}.jsonl"));
    let child_path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-01-{child_id}.jsonl"));
    let root_meta = session_meta(root_id);
    let child_meta = session_meta(child_id);
    let ancestor = message("ancestor survives");
    let removed_child = message("child rollback removes this");
    let surviving_child = message("child survives");
    let root_lines = [
        rollout_line(
            /*ordinal*/ 0,
            RolloutItem::SessionMeta(root_meta.clone()),
        ),
        rollout_line(/*ordinal*/ 1, ancestor.clone()),
    ];
    let root_contents = format!("{}\n", root_lines.join("\n"));
    fs::write(root_path.as_path(), &root_contents).expect("write root rollout");
    let child_lines = [
        rollout_line(
            /*ordinal*/ 2,
            RolloutItem::SessionMeta(child_meta.clone()),
        ),
        rollout_line(/*ordinal*/ 3, removed_child),
        rollout_line(
            /*ordinal*/ 4,
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                rollback_start_index: Some(1),
                ..Default::default()
            })),
        ),
        rollout_line(/*ordinal*/ 5, surviving_child.clone()),
    ];
    fs::write(
        child_path.as_path(),
        format!("{}\n", child_lines.join("\n")),
    )
    .expect("write child rollout");
    let lineage = RolloutLineage {
        segments: vec![
            RolloutLineageSegment {
                rollout_id: root_id,
                rollout_path: root_path,
                start_ordinal: 1,
                end: Some(HistoryPosition {
                    thread_id: root_id,
                    end_ordinal_exclusive: 2,
                    end_byte_offset: u64::try_from(root_contents.len())
                        .expect("root length fits u64"),
                }),
            },
            RolloutLineageSegment {
                rollout_id: child_id,
                rollout_path: child_path,
                start_ordinal: 3,
                end: None,
            },
        ],
    };

    let items = load_lineage(lineage, child_meta.clone())
        .await
        .expect("load lineage");

    assert_rollout_items_eq(
        &items,
        vec![
            RolloutItem::SessionMeta(child_meta),
            ancestor,
            surviving_child,
        ],
    );
}

#[tokio::test]
async fn complete_lineage_does_not_read_past_inherited_byte_cutoff() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{thread_id}.jsonl"));
    let session_meta = session_meta(thread_id);
    let inherited = message("inherited");
    let inherited_lines = [
        rollout_line(
            /*ordinal*/ 0,
            RolloutItem::SessionMeta(session_meta.clone()),
        ),
        rollout_line(/*ordinal*/ 1, inherited.clone()),
    ];
    let inherited_contents = format!("{}\n", inherited_lines.join("\n"));
    fs::write(
        path.as_path(),
        format!("{inherited_contents}{{\"timestamp\":"),
    )
    .expect("write rollout");
    let lineage = RolloutLineage {
        segments: vec![RolloutLineageSegment {
            rollout_id: thread_id,
            rollout_path: path,
            start_ordinal: 1,
            end: Some(HistoryPosition {
                thread_id,
                end_ordinal_exclusive: 2,
                end_byte_offset: u64::try_from(inherited_contents.len())
                    .expect("rollout length fits u64"),
            }),
        }],
    };

    let items = load_lineage(lineage, session_meta.clone())
        .await
        .expect("load inherited prefix");

    assert_rollout_items_eq(
        &items,
        vec![RolloutItem::SessionMeta(session_meta), inherited],
    );
}

#[tokio::test]
async fn current_segment_uses_the_opened_file_length_as_its_snapshot() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{thread_id}.jsonl"));
    let session_meta = session_meta(thread_id);
    let inherited = message("inherited");
    let appended = message("appended after snapshot");
    fs::write(
        path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(/*ordinal*/ 0, RolloutItem::SessionMeta(session_meta),),
                rollout_line(/*ordinal*/ 1, inherited.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write rollout");
    let file = codex_rollout::open_rollout_seekable_reader_without_recovery(path.as_path())
        .await
        .expect("open rollout snapshot");
    let byte_limit = file.metadata().expect("snapshot metadata").len();
    writeln!(
        OpenOptions::new()
            .append(true)
            .open(path.as_path())
            .expect("open rollout for append"),
        "{}",
        rollout_line(/*ordinal*/ 2, appended)
    )
    .expect("append rollout item");
    let segment = RolloutLineageSegment {
        rollout_id: thread_id,
        rollout_path: path,
        start_ordinal: 1,
        end: None,
    };

    let items = load_segment_from_snapshot(&segment, file, byte_limit)
        .await
        .expect("load frozen snapshot");

    assert_rollout_items_eq(&items, vec![inherited]);
}

#[tokio::test]
async fn opened_source_snapshot_survives_atomic_path_replacement() {
    let active_home = TempDir::new().expect("active home");
    let source_dir = TempDir::new().expect("source dir");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let thread_id = ThreadId::default();
    let path = source_dir.path().join("renamed-source.jsonl");
    let original_meta = session_meta(thread_id);
    let original_message = message("original snapshot");
    fs::write(
        path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 0,
                    RolloutItem::SessionMeta(original_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 1, original_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write original source");
    let source_file = codex_rollout::open_rollout_seekable_reader_without_recovery(path.as_path())
        .await
        .expect("open source snapshot");
    let source_byte_limit = source_file.metadata().expect("source metadata").len();

    let replaced_path = source_dir.path().join("replaced-source.jsonl");
    fs::rename(path.as_path(), replaced_path).expect("replace source path");
    let replacement_message = message("replacement source");
    fs::write(
        path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 0,
                    RolloutItem::SessionMeta(session_meta(thread_id)),
                ),
                rollout_line(/*ordinal*/ 1, replacement_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write replacement source");

    let source = load_opened_source(&store, path.clone(), source_file, source_byte_limit)
        .await
        .expect("load opened source");

    assert_rollout_items_eq(
        &source.history.items,
        vec![RolloutItem::SessionMeta(original_meta), original_message],
    );
    assert!(
        fs::read_to_string(path)
            .expect("read replacement source")
            .contains("replacement source")
    );
}

#[tokio::test]
async fn copied_source_ignores_an_incomplete_leaf_record() {
    let active_home = TempDir::new().expect("active home");
    let source_dir = TempDir::new().expect("source dir");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let thread_id = ThreadId::default();
    let path = source_dir.path().join("partial-source.jsonl");
    let source_meta = session_meta(thread_id);
    let source_message = message("complete source record");
    fs::write(
        path.as_path(),
        format!(
            "{}\n{{\"timestamp\":",
            [
                rollout_line(
                    /*ordinal*/ 0,
                    RolloutItem::SessionMeta(source_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 1, source_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write source with partial tail");

    let source = load(
        &store,
        LoadForkSourceByRolloutPathParams { rollout_path: path },
    )
    .await
    .expect("load complete source prefix");

    assert_rollout_items_eq(
        &source.history.items,
        vec![RolloutItem::SessionMeta(source_meta), source_message],
    );
}

#[tokio::test]
async fn copied_legacy_source_skips_rejected_records() {
    let active_home = TempDir::new().expect("active home");
    let source_dir = TempDir::new().expect("source dir");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let thread_id = ThreadId::default();
    let path = source_dir.path().join("legacy-source.jsonl");
    let mut source_meta = session_meta(thread_id);
    source_meta.meta.history_mode = ThreadHistoryMode::Legacy;
    let source_message = message("legacy source survives");
    fs::write(
        path.as_path(),
        format!(
            "{}\n{{\"malformed\":\n{}\n",
            rollout_line(
                /*ordinal*/ 0,
                RolloutItem::SessionMeta(source_meta.clone()),
            ),
            rollout_line(/*ordinal*/ 1, source_message.clone()),
        ),
    )
    .expect("write legacy source");

    let source = load(
        &store,
        LoadForkSourceByRolloutPathParams { rollout_path: path },
    )
    .await
    .expect("load tolerant legacy source");

    assert_rollout_items_eq(
        &source.history.items,
        vec![RolloutItem::SessionMeta(source_meta), source_message],
    );
}

#[tokio::test]
async fn complete_lineage_rejects_cutoff_inside_jsonl_record() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{thread_id}.jsonl"));
    let session_meta = session_meta(thread_id);
    let lines = [
        rollout_line(
            /*ordinal*/ 0,
            RolloutItem::SessionMeta(session_meta.clone()),
        ),
        rollout_line(/*ordinal*/ 1, message("inherited")),
    ];
    let contents = format!("{}\n", lines.join("\n"));
    fs::write(path.as_path(), &contents).expect("write rollout");
    let lineage = RolloutLineage {
        segments: vec![RolloutLineageSegment {
            rollout_id: thread_id,
            rollout_path: path,
            start_ordinal: 1,
            end: Some(HistoryPosition {
                thread_id,
                end_ordinal_exclusive: 2,
                end_byte_offset: u64::try_from(contents.len() - 1)
                    .expect("rollout length fits u64"),
            }),
        }],
    };

    let err = load_lineage(lineage, session_meta)
        .await
        .expect_err("partial JSONL cutoff should fail");

    assert!(
        err.to_string()
            .contains("cutoff is not at a JSONL record boundary"),
        "{err}"
    );
}

#[tokio::test]
async fn complete_lineage_classifies_rejected_records_as_invalid_requests() {
    let home = TempDir::new().expect("temp dir");
    for (suffix, rejected_record, expected_message) in [
        (
            "malformed",
            "{\"timestamp\":",
            "failed to parse paginated fork source",
        ),
        (
            "unknown",
            "{\"timestamp\":\"2026-08-24T00:00:00.000Z\",\"ordinal\":1,\"type\":\"future_item\",\"payload\":{}}",
            "failed to decode paginated fork source",
        ),
    ] {
        let thread_id = ThreadId::default();
        let path = home
            .path()
            .join(format!("rollout-{suffix}-{thread_id}.jsonl"));
        let session_meta = session_meta(thread_id);
        fs::write(
            path.as_path(),
            format!(
                "{}\n{rejected_record}\n",
                rollout_line(
                    /*ordinal*/ 0,
                    RolloutItem::SessionMeta(session_meta.clone()),
                )
            ),
        )
        .expect("write rejected rollout");
        let lineage = RolloutLineage {
            segments: vec![RolloutLineageSegment {
                rollout_id: thread_id,
                rollout_path: path,
                start_ordinal: 1,
                end: None,
            }],
        };

        let err = load_lineage(lineage, session_meta)
            .await
            .expect_err("rejected record should fail");

        assert!(
            matches!(
                &err,
                ThreadStoreError::InvalidRequest { message }
                    if message.contains(expected_message)
            ),
            "{err}"
        );
    }
}

#[tokio::test]
async fn complete_lineage_projects_latest_current_session_metadata() {
    let home = TempDir::new().expect("temp dir");
    let root_id = ThreadId::default();
    let child_id = ThreadId::default();
    let root_path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-00-{root_id}.jsonl"));
    let child_path = home
        .path()
        .join(format!("rollout-2026-08-24T00-00-01-{child_id}.jsonl"));
    let root_meta = session_meta(root_id);
    let child_meta = session_meta(child_id);
    let root_message = message("root history");
    let mut root_update = root_meta.clone();
    root_update.meta.memory_mode = Some("disabled".to_string());
    let root_contents = format!(
        "{}\n",
        [
            rollout_line(/*ordinal*/ 0, RolloutItem::SessionMeta(root_meta),),
            rollout_line(/*ordinal*/ 1, root_message.clone()),
            rollout_line(/*ordinal*/ 2, RolloutItem::SessionMeta(root_update),),
        ]
        .join("\n")
    );
    fs::write(root_path.as_path(), &root_contents).expect("write root rollout");
    let mut child_update = child_meta.clone();
    child_update.meta.memory_mode = Some("enabled".to_string());
    fs::write(
        child_path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 3,
                    RolloutItem::SessionMeta(child_meta.clone()),
                ),
                rollout_line(
                    /*ordinal*/ 4,
                    RolloutItem::SessionMeta(child_update.clone()),
                ),
            ]
            .join("\n")
        ),
    )
    .expect("write child rollout");
    let lineage = RolloutLineage {
        segments: vec![
            RolloutLineageSegment {
                rollout_id: root_id,
                rollout_path: root_path,
                start_ordinal: 1,
                end: Some(HistoryPosition {
                    thread_id: root_id,
                    end_ordinal_exclusive: 3,
                    end_byte_offset: u64::try_from(root_contents.len())
                        .expect("root length fits u64"),
                }),
            },
            RolloutLineageSegment {
                rollout_id: child_id,
                rollout_path: child_path,
                start_ordinal: 4,
                end: None,
            },
        ],
    };

    let items = load_lineage(lineage, child_meta.clone())
        .await
        .expect("load lineage");

    assert_rollout_items_eq(
        &items,
        vec![RolloutItem::SessionMeta(child_update), root_message],
    );
}

#[tokio::test]
async fn copied_fork_source_resolves_distinct_ancestor_rollout_id_from_renamed_leaf() {
    let active_home = TempDir::new().expect("active home");
    let source_home = TempDir::new().expect("source home");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let source_day = source_home
        .path()
        .join(codex_rollout::SESSIONS_SUBDIR)
        .join("2026/08/24");
    fs::create_dir_all(&source_day).expect("create source sessions");
    let child_id = ThreadId::default();
    let root_rollout_id = ThreadId::default();
    let root_path = source_day.join(format!(
        "rollout-2026-08-24T00-00-00-{child_id}_{root_rollout_id}.jsonl"
    ));
    let root_message = message("renamed source root");
    let root_contents = format!(
        "{}\n",
        [
            rollout_line(
                /*ordinal*/ 0,
                RolloutItem::SessionMeta(session_meta(child_id)),
            ),
            rollout_line(/*ordinal*/ 1, root_message.clone()),
        ]
        .join("\n")
    );
    fs::write(root_path, &root_contents).expect("write root rollout");
    let path = source_day.join("renamed-backup.jsonl");
    let mut child_meta = session_meta(child_id);
    child_meta.meta.history_base = Some(HistoryPosition {
        thread_id: root_rollout_id,
        end_ordinal_exclusive: 2,
        end_byte_offset: u64::try_from(root_contents.len()).expect("root length fits u64"),
    });
    let child_message = message("renamed source child");
    fs::write(
        path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 2,
                    RolloutItem::SessionMeta(child_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 3, child_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write renamed rollout");

    let source = load(
        &store,
        LoadForkSourceByRolloutPathParams { rollout_path: path },
    )
    .await
    .expect("load renamed rollout");

    assert_eq!(source.history.thread_id, child_id);
    assert_rollout_items_eq(
        &source.history.items,
        vec![
            RolloutItem::SessionMeta(child_meta),
            root_message,
            child_message,
        ],
    );
}

#[tokio::test]
async fn copied_lineage_reads_backup_only_ancestor_without_mutating_source() {
    let active_home = TempDir::new().expect("active home");
    let source_home = TempDir::new().expect("source home");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let source_day = source_home
        .path()
        .join(codex_rollout::SESSIONS_SUBDIR)
        .join("2026/08/24");
    fs::create_dir_all(&source_day).expect("create source sessions");
    let root_id = ThreadId::default();
    let child_id = ThreadId::default();
    let root_path = source_day.join(format!("rollout-2026-08-24T00-00-00-{root_id}.jsonl"));
    let compacted = RolloutItem::Compacted(CompactedItem {
        message: "recoverable checkpoint".to_string(),
        replacement_history: Some(vec![ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,recoverable".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })]),
        ..Default::default()
    });
    let root_contents = format!(
        "{}\n",
        [
            rollout_line(
                /*ordinal*/ 0,
                RolloutItem::SessionMeta(session_meta(root_id)),
            ),
            rollout_line(/*ordinal*/ 1, compacted.clone()),
        ]
        .join("\n")
    );
    let root_file_name = root_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("root rollout filename");
    let backup_path = root_path.with_file_name(format!(
        ".{root_file_name}.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::write(backup_path.as_path(), &root_contents).expect("write root recovery backup");

    let mut child_meta = session_meta(child_id);
    child_meta.meta.history_base = Some(HistoryPosition {
        thread_id: root_id,
        end_ordinal_exclusive: 2,
        end_byte_offset: u64::try_from(root_contents.len()).expect("root length fits u64"),
    });
    let child_message = message("child after backup-only ancestor");
    let child_path = source_day.join(format!("rollout-2026-08-24T00-00-01-{child_id}.jsonl"));
    fs::write(
        child_path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 2,
                    RolloutItem::SessionMeta(child_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 3, child_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write child rollout");
    let backup_contents = fs::read(backup_path.as_path()).expect("read root recovery backup");

    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(source_day.as_path())
            .expect("source directory metadata")
            .permissions();
        let original_mode = permissions.mode();
        permissions.set_mode(original_mode & !0o222);
        fs::set_permissions(source_day.as_path(), permissions)
            .expect("make source directory read only");
        original_mode
    };

    let source = load(
        &store,
        LoadForkSourceByRolloutPathParams {
            rollout_path: child_path,
        },
    )
    .await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(
            source_day.as_path(),
            fs::Permissions::from_mode(original_mode),
        )
        .expect("restore source directory permissions");
    }

    let source = source.expect("load backup-only lineage");

    assert_rollout_items_eq(
        &source.history.items,
        vec![
            RolloutItem::SessionMeta(child_meta),
            compacted,
            child_message,
        ],
    );
    assert!(!root_path.exists());
    assert_eq!(
        fs::read(backup_path).expect("read retained recovery backup"),
        backup_contents
    );
}

#[tokio::test]
async fn copied_lineage_keeps_opened_ancestor_after_path_replacement() {
    let source_home = TempDir::new().expect("source home");
    let source_store =
        LocalThreadStore::new(test_config(source_home.path()), /*state_db*/ None);
    let source_day = source_home
        .path()
        .join(codex_rollout::SESSIONS_SUBDIR)
        .join("2026/08/24");
    fs::create_dir_all(&source_day).expect("create source sessions");
    let root_id = ThreadId::default();
    let child_id = ThreadId::default();
    let root_path = source_day.join(format!("rollout-2026-08-24T00-00-00-{root_id}.jsonl"));
    let root_message = message("opened ancestor");
    let root_contents = format!(
        "{}\n",
        [
            rollout_line(
                /*ordinal*/ 0,
                RolloutItem::SessionMeta(session_meta(root_id)),
            ),
            rollout_line(/*ordinal*/ 1, root_message.clone()),
        ]
        .join("\n")
    );
    fs::write(root_path.as_path(), &root_contents).expect("write root rollout");
    let mut child_meta = session_meta(child_id);
    child_meta.meta.history_base = Some(HistoryPosition {
        thread_id: root_id,
        end_ordinal_exclusive: 2,
        end_byte_offset: u64::try_from(root_contents.len()).expect("root length fits u64"),
    });
    let child_message = message("child");
    let child_path = source_day.join(format!("rollout-2026-08-24T00-00-01-{child_id}.jsonl"));
    fs::write(
        child_path.as_path(),
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 2,
                    RolloutItem::SessionMeta(child_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 3, child_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write child rollout");
    let child_file = std::fs::File::open(child_path.as_path()).expect("open child snapshot");
    let child_byte_limit = child_file.metadata().expect("child metadata").len();
    let lineage = source_store
        .resolve_rollout_lineage_from_snapshot(
            child_id,
            child_path,
            child_meta.clone(),
            child_file,
            child_byte_limit,
        )
        .await
        .expect("open complete lineage");

    let original_root = source_day.join("opened-root.jsonl");
    fs::rename(root_path.as_path(), original_root).expect("replace ancestor path");
    fs::write(
        root_path,
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 0,
                    RolloutItem::SessionMeta(session_meta(root_id)),
                ),
                rollout_line(/*ordinal*/ 1, message("replacement ancestor")),
            ]
            .join("\n")
        ),
    )
    .expect("write replacement ancestor");

    let (_canonical_meta, items) = load_complete_lineage(lineage, child_meta.clone())
        .await
        .expect("load opened lineage");

    assert_rollout_items_eq(
        &items,
        vec![
            RolloutItem::SessionMeta(child_meta),
            root_message,
            child_message,
        ],
    );
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_rollout_home_wins_for_file_and_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let active_home = TempDir::new().expect("active home");
    let source_home = TempDir::new().expect("source home");
    let store = LocalThreadStore::new(test_config(active_home.path()), /*state_db*/ None);
    let source_sessions = source_home.path().join(codex_rollout::SESSIONS_SUBDIR);
    let source_day = source_sessions.join("2026/08/24");
    fs::create_dir_all(&source_day).expect("create source sessions");
    let root_id = ThreadId::default();
    let child_id = ThreadId::default();
    let root_path = source_day.join(format!("rollout-2026-08-24T00-00-00-{root_id}.jsonl"));
    let root_meta = session_meta(root_id);
    let root_message = message("root");
    let root_contents = format!(
        "{}\n",
        [
            rollout_line(/*ordinal*/ 0, RolloutItem::SessionMeta(root_meta),),
            rollout_line(/*ordinal*/ 1, root_message.clone()),
        ]
        .join("\n")
    );
    fs::write(&root_path, &root_contents).expect("write root rollout");
    let mut child_meta = session_meta(child_id);
    child_meta.meta.history_base = Some(HistoryPosition {
        thread_id: root_id,
        end_ordinal_exclusive: 2,
        end_byte_offset: u64::try_from(root_contents.len()).expect("root length fits u64"),
    });
    let child_path = source_day.join(format!("rollout-2026-08-24T00-00-01-{child_id}.jsonl"));
    let child_message = message("child");
    fs::write(
        &child_path,
        format!(
            "{}\n",
            [
                rollout_line(
                    /*ordinal*/ 2,
                    RolloutItem::SessionMeta(child_meta.clone()),
                ),
                rollout_line(/*ordinal*/ 3, child_message.clone()),
            ]
            .join("\n")
        ),
    )
    .expect("write child rollout");
    let expected_items = vec![
        RolloutItem::SessionMeta(child_meta),
        root_message,
        child_message,
    ];

    let active_sessions = active_home.path().join(codex_rollout::SESSIONS_SUBDIR);
    fs::create_dir_all(&active_sessions).expect("create active sessions");
    let file_link = active_sessions.join("file-link.jsonl");
    symlink(&child_path, &file_link).expect("link source rollout");
    let file_history = load(
        &store,
        LoadForkSourceByRolloutPathParams {
            rollout_path: file_link,
        },
    )
    .await
    .expect("load file symlink");
    assert_eq!(file_history.history.thread_id, child_id);
    assert_rollout_items_eq(&file_history.history.items, expected_items.clone());

    let directory_link = active_sessions.join("linked-day");
    symlink(&source_day, &directory_link).expect("link source sessions");
    let directory_history = load(
        &store,
        LoadForkSourceByRolloutPathParams {
            rollout_path: directory_link
                .join(child_path.file_name().expect("child rollout filename")),
        },
    )
    .await
    .expect("load directory symlink");
    assert_eq!(directory_history.history.thread_id, child_id);
    assert_rollout_items_eq(&directory_history.history.items, expected_items);
}

async fn load_lineage(
    lineage: RolloutLineage,
    session_meta: SessionMetaLine,
) -> super::super::ThreadStoreResult<Vec<RolloutItem>> {
    let segments = lineage
        .segments
        .into_iter()
        .map(|segment| {
            let file =
                std::fs::File::open(segment.rollout_path.as_path()).expect("open rollout snapshot");
            let byte_limit = file.metadata().expect("source metadata").len();
            OpenedRolloutLineageSegment {
                segment,
                file,
                byte_limit,
            }
        })
        .collect();
    load_complete_lineage(OpenedRolloutLineage { segments }, session_meta)
        .await
        .map(|(_session_meta, items)| items)
}

fn assert_rollout_items_eq(actual: &[RolloutItem], expected: Vec<RolloutItem>) {
    assert_eq!(
        serde_json::to_value(actual).expect("serialize actual rollout items"),
        serde_json::to_value(expected).expect("serialize expected rollout items"),
    );
}

fn session_meta(thread_id: ThreadId) -> SessionMetaLine {
    SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            history_mode: ThreadHistoryMode::Paginated,
            ..SessionMeta::default()
        },
        git: None,
    }
}

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn rollout_line(ordinal: u64, item: RolloutItem) -> String {
    serde_json::to_string(&RolloutLine {
        timestamp: "2026-08-24T00:00:00.000Z".to_string(),
        ordinal: Some(ordinal),
        item,
    })
    .expect("serialize rollout line")
}
