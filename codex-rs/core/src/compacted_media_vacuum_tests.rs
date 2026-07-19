use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_rollout::WriterLockCoordinator;
use codex_rollout::try_acquire_rollout_maintenance_lock;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::vacuum_rollout_compacted_media;

fn write_rollout(
    directory: &Path,
    filename_thread: ThreadId,
    metadata_thread: ThreadId,
) -> PathBuf {
    fs::create_dir_all(directory).expect("create rollout directory");
    let path = directory.join(format!(
        "rollout-2026-01-01T00-00-00-{filename_thread}.jsonl"
    ));
    let records = [
        json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": metadata_thread,
                "session_id": metadata_thread,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": directory,
                "originator": "vacuum-test",
                "cli_version": "test"
            }
        }),
        json!({
            "timestamp": "2026-01-01T00:00:01Z",
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
            "timestamp": "2026-01-01T00:00:02Z",
            "type": "compacted",
            "payload": {
                "message": "repair",
                "replacement_history_media_sanitized_prefix_len": 0,
                "replacement_history": []
            }
        }),
    ];
    let mut contents = Vec::new();
    for record in records {
        serde_json::to_writer(&mut contents, &record).expect("serialize rollout record");
        contents.push(b'\n');
    }
    fs::write(&path, contents).expect("write rollout");
    path
}

fn update_session_metadata(path: &Path, fields: Value) {
    let contents = fs::read_to_string(path).expect("rollout contents");
    let mut records: Vec<Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).expect("rollout record"))
        .collect();
    records[0]["payload"]
        .as_object_mut()
        .expect("metadata object")
        .extend(fields.as_object().expect("metadata fields").clone());
    if fields["history_mode"] == "paginated" {
        let first_ordinal = fields["history_base"]["end_ordinal_exclusive"]
            .as_u64()
            .unwrap_or(0);
        for (index, record) in records.iter_mut().enumerate() {
            record["ordinal"] = json!(first_ordinal + index as u64);
        }
    }
    let contents = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{contents}\n")).expect("updated metadata");
}

fn seed_vacuum_artifacts(path: &Path) {
    let filename = path.file_name().expect("filename").to_str().expect("UTF-8");
    let backup_id = ThreadId::new();
    for (name, contents) in [
        (
            format!(".{filename}.media-vacuum.json"),
            b"retained manifest".as_slice(),
        ),
        (
            format!(".{filename}.pre-media-vacuum-{backup_id}.bak"),
            b"retained backup".as_slice(),
        ),
        (
            format!(".{filename}.media-vacuum-interrupted.tmp"),
            b"retained temporary".as_slice(),
        ),
    ] {
        fs::write(path.with_file_name(name), contents).expect("existing vacuum artifact");
    }
}

fn directory_contents(directory: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(directory)
        .expect("rollout directory")
        .map(|entry| {
            let entry = entry.expect("directory entry");
            (
                entry.file_name(),
                fs::read(entry.path()).expect("file bytes"),
            )
        })
        .collect()
}

#[tokio::test]
async fn active_writer_prevents_rewrite_and_releases_maintenance() {
    let home = TempDir::new().expect("home");
    let thread = ThreadId::new();
    let path = write_rollout(&home.path().join("sessions"), thread, thread);
    let original = fs::read(&path).expect("original");
    let coordinator = Arc::new(WriterLockCoordinator::new(home.path()));
    let writer = coordinator.acquire(thread).expect("active writer");

    let error = vacuum_rollout_compacted_media(home.path(), &path)
        .await
        .expect_err("active writer must exclude vacuum");

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read(&path).expect("unchanged rollout"), original);
    assert!(
        try_acquire_rollout_maintenance_lock(home.path())
            .expect("maintenance lock")
            .is_some()
    );
    drop(writer);
    let report = vacuum_rollout_compacted_media(home.path(), &path)
        .await
        .expect("closed rollout");
    assert_eq!(report.omitted_image_count, 1);
    assert!(coordinator.acquire(thread).is_ok());
}

#[tokio::test]
async fn competing_maintenance_prevents_rewrite() {
    let home = TempDir::new().expect("home");
    let thread = ThreadId::new();
    let path = write_rollout(&home.path().join("sessions"), thread, thread);
    let original = fs::read(&path).expect("original");
    let _maintenance = try_acquire_rollout_maintenance_lock(home.path())
        .expect("maintenance lock")
        .expect("exclusive maintenance");

    let error = vacuum_rollout_compacted_media(home.path(), &path)
        .await
        .expect_err("maintenance must exclude vacuum");

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read(&path).expect("unchanged rollout"), original);
    let coordinator = Arc::new(WriterLockCoordinator::new(home.path()));
    assert!(coordinator.acquire(thread).is_ok());
}

#[tokio::test]
async fn rejects_foreign_home_and_mismatched_metadata_without_rewrite() {
    let home = TempDir::new().expect("home");
    let foreign_home = TempDir::new().expect("foreign home");
    let thread = ThreadId::new();
    let foreign_path = write_rollout(&foreign_home.path().join("sessions"), thread, thread);
    let mismatched_path = write_rollout(&home.path().join("sessions"), thread, ThreadId::new());

    for (path, expected_kind) in [
        (foreign_path, io::ErrorKind::InvalidInput),
        (mismatched_path, io::ErrorKind::InvalidData),
    ] {
        let original = fs::read(&path).expect("original");
        let error = vacuum_rollout_compacted_media(home.path(), &path)
            .await
            .expect_err("unproven ownership");
        assert_eq!(error.kind(), expected_kind);
        assert_eq!(fs::read(&path).expect("unchanged rollout"), original);
    }
}

#[tokio::test]
async fn rejects_identity_record_that_extends_beyond_the_probe_without_rewrite() {
    let home = TempDir::new().expect("home");
    let thread = ThreadId::new();
    let path = write_rollout(&home.path().join("sessions"), thread, thread);
    let original = fs::read(&path).expect("original");
    let newline = original
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("metadata delimiter");
    let mut oversized = original[..newline].to_vec();
    // A valid JSON prefix is insufficient: the rest of the same record is not yet proven.
    oversized.extend(vec![b' '; 1024 * 1024]);
    oversized.extend_from_slice(b"invalid trailing metadata");
    oversized.extend_from_slice(&original[newline..]);
    fs::write(&path, &oversized).expect("oversized metadata");

    let error = vacuum_rollout_compacted_media(home.path(), &path)
        .await
        .expect_err("incomplete identity record");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path).expect("unchanged rollout"), oversized);
}

#[tokio::test]
async fn legacy_compressed_archive_accepts_revert_shaped_filename_with_stable_identity() {
    for extension in ["jsonl", "jsonl.zst"] {
        let home = TempDir::new().expect("home");
        let thread = ThreadId::new();
        let path = write_rollout(&home.path().join("archived_sessions"), thread, thread);
        let rollout = ThreadId::new();
        let reverted = path.with_file_name(format!(
            "rollout-2026-01-01T00-00-00-{thread}_{rollout}.jsonl"
        ));
        let compressed = zstd::stream::encode_all(
            fs::read(&path).expect("original").as_slice(),
            /*level*/ 0,
        )
        .expect("compress");
        fs::write(reverted.with_extension("jsonl.zst"), compressed).expect("compressed rollout");
        fs::remove_file(&path).expect("remove original");
        let selected = reverted.with_extension(extension);
        let coordinator = Arc::new(WriterLockCoordinator::new(home.path()));
        let writer = coordinator.acquire(thread).expect("stable thread writer");
        assert_eq!(
            vacuum_rollout_compacted_media(home.path(), &selected)
                .await
                .expect_err("stable thread is active")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);

        let report = vacuum_rollout_compacted_media(home.path(), &selected)
            .await
            .expect("closed reverted archive");

        assert_eq!(report.omitted_image_count, 1);
    }
}

#[tokio::test]
async fn paginated_sources_are_rejected_with_or_without_existing_descendants() {
    for extension in ["jsonl", "jsonl.zst"] {
        for descendants in ["none", "referenced"] {
            let home = TempDir::new().expect("home");
            let directory = home.path().join("sessions");
            let thread = ThreadId::new();
            let path = write_rollout(&directory, thread, thread);
            update_session_metadata(&path, json!({"history_mode": "paginated"}));
            let source = fs::read(&path).expect("source bytes");
            if descendants == "referenced" {
                let child_thread = ThreadId::new();
                let child = write_rollout(&directory, child_thread, child_thread);
                update_session_metadata(
                    &child,
                    json!({
                        "history_mode": "paginated",
                        "history_base": {
                            "thread_id": thread,
                            "end_ordinal_exclusive": 3,
                            "end_byte_offset": source.len()
                        }
                    }),
                );
            }
            seed_vacuum_artifacts(&path);
            let selected = path.with_extension(extension);
            if extension == "jsonl.zst" {
                let compressed = zstd::stream::encode_all(source.as_slice(), /*level*/ 0)
                    .expect("compress source");
                fs::write(&selected, compressed).expect("compressed source");
                fs::remove_file(&path).expect("compressed-only source");
            }
            let before = directory_contents(&directory);

            let error = vacuum_rollout_compacted_media(home.path(), &selected)
                .await
                .expect_err("Paginated offsets must not be rewritten");

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            // Includes source, reference child, and every existing recovery artifact, and
            // catches accidental compressed-source materialization or new recovery artifacts.
            assert_eq!(directory_contents(&directory), before);
        }
    }
}

#[tokio::test]
async fn legacy_metadata_with_lineage_is_rejected_without_artifact_changes() {
    for fields in [
        json!({
            "history_mode": "legacy",
            "history_base": {
                "thread_id": ThreadId::new(),
                "end_ordinal_exclusive": 3,
                "end_byte_offset": 1024
            }
        }),
        json!({"history_mode": "legacy", "subagent_history_start_ordinal": 0}),
    ] {
        for extension in ["jsonl", "jsonl.zst"] {
            let home = TempDir::new().expect("home");
            let directory = home.path().join("sessions");
            let thread = ThreadId::new();
            let path = write_rollout(&directory, thread, thread);
            update_session_metadata(&path, fields.clone());
            seed_vacuum_artifacts(&path);
            let selected = path.with_extension(extension);
            if extension == "jsonl.zst" {
                let source = fs::read(&path).expect("source");
                let compressed = zstd::stream::encode_all(source.as_slice(), /*level*/ 0)
                    .expect("compress source");
                fs::write(&selected, compressed).expect("compressed source");
                fs::remove_file(&path).expect("compressed-only source");
            }
            let before = directory_contents(&directory);

            let error = vacuum_rollout_compacted_media(home.path(), &selected)
                .await
                .expect_err("Legacy lineage is not standalone");

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(directory_contents(&directory), before);
        }
    }
}

#[tokio::test]
async fn accepts_relative_rollout_path_without_changing_cwd() {
    let cwd = std::env::current_dir().expect("cwd");
    let home = TempDir::new_in(&cwd).expect("home beneath cwd");
    let thread = ThreadId::new();
    let path = write_rollout(&home.path().join("sessions"), thread, thread);
    let relative = path.strip_prefix(&cwd).expect("relative path");

    let report = vacuum_rollout_compacted_media(home.path(), relative)
        .await
        .expect("relative rollout");

    assert_eq!(report.omitted_image_count, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinked_rollout_and_escaped_session_directory() {
    let home = TempDir::new().expect("home");
    let foreign_home = TempDir::new().expect("foreign home");
    let thread = ThreadId::new();
    let foreign_path = write_rollout(&foreign_home.path().join("sessions"), thread, thread);
    let original = fs::read(&foreign_path).expect("original");
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let linked_file = sessions.join(foreign_path.file_name().expect("filename"));
    std::os::unix::fs::symlink(&foreign_path, &linked_file).expect("symlink rollout");
    let linked_directory = sessions.join("foreign");
    std::os::unix::fs::symlink(foreign_path.parent().expect("parent"), &linked_directory)
        .expect("symlink directory");

    for selected in [
        linked_file,
        linked_directory.join(foreign_path.file_name().expect("filename")),
    ] {
        assert_eq!(
            vacuum_rollout_compacted_media(home.path(), &selected)
                .await
                .expect_err("symlink must not bypass authority")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        fs::read(&foreign_path).expect("unchanged rollout"),
        original
    );
}

#[tokio::test]
async fn cancelling_caller_keeps_worker_maintenance_until_publication_finishes() {
    let home = TempDir::new().expect("home");
    let thread = ThreadId::new();
    let path = write_rollout(&home.path().join("sessions"), thread, thread);
    let locks = home.path().join("thread-writer-locks");
    fs::create_dir(&locks).expect("writer lock directory");
    let coordination = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(locks.join(".coordination.lock"))
        .expect("coordination gate");
    coordination
        .lock()
        .expect("hold worker before thread acquisition");
    let worker_home = home.path().to_path_buf();
    let worker_path = path.clone();
    let caller =
        tokio::spawn(
            async move { vacuum_rollout_compacted_media(&worker_home, &worker_path).await },
        );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            if try_acquire_rollout_maintenance_lock(home.path())
                .expect("probe maintenance")
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("worker owns maintenance");

    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    assert!(
        try_acquire_rollout_maintenance_lock(home.path())
            .expect("maintenance remains owned")
            .is_none()
    );
    drop(coordination);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            if try_acquire_rollout_maintenance_lock(home.path())
                .expect("probe worker completion")
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("worker finished publication");
    let contents = fs::read_to_string(&path).expect("published rollout");
    assert!(!contents.contains("data:image/png;base64,legacy"));
    let coordinator = Arc::new(WriterLockCoordinator::new(home.path()));
    assert!(coordinator.acquire(thread).is_ok());
}
