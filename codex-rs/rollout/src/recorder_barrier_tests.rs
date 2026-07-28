use super::*;
use codex_protocol::protocol::ThreadRolledBackEvent;

#[tokio::test]
async fn writer_recovery_reconciles_ordinal_after_complete_unterminated_write()
-> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[4])?;

    let committed_marker = RolloutLine {
        timestamp: "2026-07-09T00:00:05Z".to_string(),
        ordinal: Some(5),
        item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: None,
        })),
    };
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    write!(file, "{}", serde_json::to_string(&committed_marker)?)?;
    drop(file);

    // Model the writer state after write_all persisted the JSON object but failed on its newline.
    let mut state = RolloutWriterState {
        writer_lock: None,
        writer: None,
        deferred_creation: false,
        pending_items: vec![agent_message_item("after-recovery")],
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Paginated { next: Some(5) },
        last_logged_error: Some("injected partial write".to_string()),
    };

    state.flush().await?;

    let lines = read_rollout_lines(&rollout_path)?;
    assert_eq!(
        lines.iter().map(|line| line.ordinal).collect::<Vec<_>>(),
        vec![Some(0), Some(4), Some(5), Some(6)]
    );
    assert!(fs::read_to_string(&rollout_path)?.ends_with('\n'));
    Ok(())
}

#[tokio::test]
async fn writer_recovery_reconciles_initial_metadata_write() -> std::io::Result<()> {
    for (name, complete_record) in [("complete", true), ("partial", false)] {
        let home = TempDir::new().expect("temp dir");
        let rollout_path = home.path().join(format!("{name}.jsonl"));
        let thread_id = ThreadId::new();
        let session_meta_item = paginated_session_meta_item(thread_id, home.path());
        let RolloutItem::SessionMeta(session_meta_line) = session_meta_item.clone() else {
            panic!("fixture should contain session metadata");
        };
        if complete_record {
            let line = RolloutLine {
                timestamp: "2026-07-09T00:00:00Z".to_string(),
                ordinal: Some(0),
                item: session_meta_item,
            };
            fs::write(&rollout_path, serde_json::to_vec(&line)?)?;
        } else {
            fs::write(&rollout_path, b"{\"timestamp\":\"2026-07-09")?;
        }

        let mut state = RolloutWriterState {
            writer_lock: None,
            writer: None,
            deferred_creation: false,
            pending_items: Vec::new(),
            meta: Some(session_meta_line.meta),
            cwd: home.path().to_path_buf(),
            rollout_path: rollout_path.clone(),
            ordinal_state: RolloutOrdinalState::Paginated { next: Some(0) },
            last_logged_error: Some("injected initial write failure".to_string()),
        };

        state.flush().await?;

        let lines = read_rollout_lines(&rollout_path)?;
        assert_eq!(
            lines.len(),
            1,
            "{name} metadata recovery duplicated a record"
        );
        assert_eq!(lines[0].ordinal, Some(0));
        assert!(matches!(&lines[0].item, RolloutItem::SessionMeta(_)));
    }
    Ok(())
}

#[tokio::test]
async fn accepted_marker_finishes_after_its_waiter_is_dropped() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[])?;
    let marker = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 0,
        materialized_turns: Some(1),
        rollback_start_index: Some(1),
    }));
    let state = RolloutWriterState {
        writer_lock: None,
        writer: None,
        deferred_creation: false,
        pending_items: Vec::new(),
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Paginated { next: Some(1) },
        last_logged_error: None,
    };
    let (tx, rx) = mpsc::channel(/*buffer*/ 2);
    let (ack, abandoned) = oneshot::channel();
    tx.send(RolloutCmd::AddItemAndFlush {
        item: Box::new(marker.clone()),
        ack,
    })
    .await
    .expect("enqueue accepted marker");
    drop(abandoned);
    let (ack, completed) = oneshot::channel();
    tx.send(RolloutCmd::Shutdown { ack })
        .await
        .expect("enqueue shutdown barrier");
    let writer = tokio::spawn(rollout_writer(state, rx));
    completed.await.expect("shutdown acknowledgement")?;
    writer.await.expect("writer task")?;
    let lines = read_rollout_lines(&rollout_path)?;
    assert_eq!(
        serde_json::to_value(
            lines
                .into_iter()
                .skip(1)
                .map(|line| (line.ordinal, line.item))
                .collect::<Vec<_>>()
        )
        .expect("actual"),
        serde_json::to_value(vec![(Some(1), marker)]).expect("expected"),
    );
    Ok(())
}

#[tokio::test]
async fn partial_marker_is_not_completed_by_a_later_flush() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[4])?;
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    file.write_all(b"{\"timestamp\":\"partial-marker")?;
    drop(file);
    let following = agent_message_item("after-partial-marker");
    let mut state = RolloutWriterState {
        writer_lock: None,
        writer: None,
        deferred_creation: false,
        pending_items: vec![following.clone()],
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Paginated { next: Some(5) },
        last_logged_error: None,
    };
    state.flush().await?;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(errors, 1);
    assert_eq!(
        serde_json::to_value(items.last()).expect("actual"),
        serde_json::to_value(Some(&following)).expect("expected"),
    );
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(
            &rollout_path,
            fs::metadata(&rollout_path)?.len(),
        )?,
        Some(5)
    );
    Ok(())
}

#[tokio::test]
async fn single_item_barrier_does_not_write_initial_session_metadata() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    let err = recorder
        .record_canonical_item_and_flush(&agent_message_item("rejected-item"))
        .await
        .expect_err("the special barrier must not create a rollout");

    assert!(err.to_string().contains("persisted session metadata"));
    assert!(!rollout_path.exists());
    recorder.shutdown().await
}

#[tokio::test]
async fn single_item_barrier_does_not_initialize_an_empty_resumed_file() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let recorder = RolloutRecorder::new(
        &test_config(home.path()),
        RolloutRecorderParams::resume(rollout_path.clone()),
    )
    .await?;
    assert!(
        recorder
            .record_canonical_item_and_flush(&agent_message_item("not-a-header"))
            .await
            .is_err()
    );
    recorder.flush().await?;
    assert_eq!(fs::read(&rollout_path)?, Vec::<u8>::new());
    recorder.shutdown().await
}

#[tokio::test]
async fn failed_single_item_barrier_does_not_retry_after_filesystem_recovers() -> std::io::Result<()>
{
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[])?;
    let before = fs::read(&rollout_path)?;
    let read_only_file = std::fs::OpenOptions::new().read(true).open(&rollout_path)?;
    let mut state = RolloutWriterState {
        writer_lock: None,
        writer: Some(JsonlWriter {
            file: tokio::fs::File::from_std(read_only_file),
        }),
        deferred_creation: false,
        pending_items: Vec::new(),
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Paginated { next: Some(1) },
        last_logged_error: None,
    };

    state
        .add_item_and_flush(agent_message_item("rejected-item"))
        .await
        .expect_err("read-only writer should reject the item");

    state.flush().await?;
    assert_eq!(fs::read(&rollout_path)?, before);
    Ok(())
}
