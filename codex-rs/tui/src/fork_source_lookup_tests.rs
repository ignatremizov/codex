use super::from_rollout_path;
use super::lookup_in_source_home;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn direct_rollout_lookup_uses_source_metadata() {
    let temp = TempDir::new().expect("temporary source home");
    let thread_id = ThreadId::new();
    let path = temp.path().join("rollout.jsonl");
    let cwd = temp.path().join("source-project");
    let payload = serde_json::json!({
        "timestamp": "2025-01-05T12:00:00Z",
        "type": "session_meta",
        "payload": {
            "session_id": thread_id,
            "id": thread_id,
            "timestamp": "2025-01-05T12:00:00Z",
            "cwd": cwd,
            "originator": "codex",
            "cli_version": "0.0.0",
            "source": "cli",
            "history_mode": "legacy"
        }
    });
    let contents = format!("{payload}\n");
    std::fs::write(&path, &contents).expect("write source rollout");

    let target = from_rollout_path(&path, Some(&thread_id.to_string()))
        .await
        .expect("lookup source rollout")
        .expect("source metadata");
    assert_eq!(
        (
            target.thread_id,
            target.cwd,
            target.history_mode,
            target.path,
            target.source_rollout_path,
        ),
        (
            thread_id,
            Some(cwd),
            Some(codex_app_server_protocol::ThreadHistoryMode::Legacy),
            Some(path.clone()),
            Some(path.clone()),
        )
    );
    assert_eq!(
        std::fs::read(&path).expect("read source rollout"),
        contents.as_bytes()
    );
    for expected_id in ["invalid-thread-id".to_string(), ThreadId::new().to_string()] {
        assert!(
            from_rollout_path(&path, Some(&expected_id))
                .await
                .expect("read source metadata")
                .is_none()
        );
    }
}

#[tokio::test]
async fn source_home_lookup_finds_active_and_archived_rollouts() {
    let temp = TempDir::new().expect("temporary source home");
    let active_id = ThreadId::new();
    let archived_id = ThreadId::new();
    let active = temp.path().join(format!(
        "sessions/2025/01/01/rollout-2025-01-01T00-00-00-{active_id}.jsonl"
    ));
    let archived = temp.path().join(format!(
        "archived_sessions/rollout-2025-01-01T00-00-00-{archived_id}.jsonl"
    ));
    write_metadata(&active, active_id, "legacy");
    write_metadata(&archived, archived_id, "paginated");

    let active_target = lookup_in_source_home(temp.path(), &active_id.to_string())
        .await
        .expect("active lookup")
        .expect("active target");
    let archived_target = lookup_in_source_home(temp.path(), &archived_id.to_string())
        .await
        .expect("archived lookup")
        .expect("archived target");
    assert_eq!(active_target.thread_id, active_id);
    assert_eq!(archived_target.thread_id, archived_id);
    assert_eq!(
        archived_target.history_mode,
        Some(codex_app_server_protocol::ThreadHistoryMode::Paginated)
    );
}

fn write_metadata(path: &std::path::Path, thread_id: ThreadId, history_mode: &str) {
    std::fs::create_dir_all(path.parent().expect("rollout parent")).expect("create rollout parent");
    let payload = serde_json::json!({
        "timestamp": "2025-01-05T12:00:00Z",
        "type": "session_meta",
        "payload": {
            "session_id": thread_id,
            "id": thread_id,
            "timestamp": "2025-01-05T12:00:00Z",
            "cwd": path.parent().expect("source directory"),
            "originator": "codex",
            "cli_version": "0.0.0",
            "source": "cli",
            "history_mode": history_mode
        }
    });
    std::fs::write(path, format!("{payload}\n")).expect("write source rollout");
}
