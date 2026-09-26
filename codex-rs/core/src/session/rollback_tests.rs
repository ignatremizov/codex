use super::*;
use pretty_assertions::assert_eq;

async fn assert_rollback_error(
    events: &async_channel::Receiver<Event>,
    expected: codex_protocol::protocol::CodexErrorInfo,
) {
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("rollback event timeout")
        .expect("rollback should emit an event");
    assert_eq!(event.id, "sub-1");
    match event.msg {
        EventMsg::Error(error) => assert_eq!(error.codex_error_info, Some(expected)),
        other => panic!("expected rollback error, got {other:?}"),
    }
}

async fn assert_rollback_success(events: &async_channel::Receiver<Event>, expected_num_turns: u32) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.expect("rollback event channel closed");
            match event.msg {
                EventMsg::TokenCount(_) => {}
                EventMsg::ThreadRolledBack(rollback) => {
                    assert_eq!(event.id, "sub-1");
                    assert_eq!(rollback.num_turns, expected_num_turns);
                    break;
                }
                other => panic!("expected rollback success event, got {other:?}"),
            }
        }
    })
    .await
    .expect("rollback event timeout");
}

async fn persisted_legacy_session() -> (
    Arc<Session>,
    Arc<TurnContext>,
    PathBuf,
    async_channel::Receiver<Event>,
) {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    session
        .state
        .lock()
        .await
        .session_configuration
        .history_mode = codex_protocol::protocol::ThreadHistoryMode::Legacy;
    let rollout_path = attach_thread_persistence(
        Arc::get_mut(&mut session).expect("session should not have additional references"),
    )
    .await;
    assert_eq!(
        session
            .state
            .lock()
            .await
            .session_configuration
            .history_mode,
        codex_protocol::protocol::ThreadHistoryMode::Legacy
    );
    (session, turn_context, rollout_path, events)
}

async fn install_two_turn_history(session: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    let initial_context = build_initial_context(session, turn_context).await;
    let mut history = initial_context;
    history.extend([
        user_message("turn 1 user"),
        assistant_message("turn 1 assistant"),
        user_message("turn 2 user"),
        assistant_message("turn 2 assistant"),
    ]);
    session
        .replace_history(history.clone(), Some(turn_context.to_turn_context_item()))
        .await;
    session
        .persist_rollout_items(
            &history
                .into_iter()
                .map(ResponseItemEnvelope::new)
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>(),
        )
        .await;
}

#[tokio::test]
async fn thread_rollback_drops_last_turn_from_history() {
    let (session, turn_context, rollout_path, events) = persisted_legacy_session().await;
    install_two_turn_history(&session, &turn_context).await;
    let initial_context = build_initial_context(&session, &turn_context).await;

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_success(&events, 1).await;

    let mut expected = initial_context;
    expected.extend([
        user_message("turn 1 user"),
        assistant_message("turn 1 assistant"),
    ]);
    assert_eq!(expected, raw_history_items(&session.clone_history().await));
    assert!(matches!(
        RolloutRecorder::get_rollout_history(&rollout_path).await,
        Ok(InitialHistory::Resumed(_))
    ));
}

#[tokio::test]
async fn thread_rollback_clears_history_when_num_turns_exceeds_existing_turns() {
    let (session, turn_context, _rollout_path, events) = persisted_legacy_session().await;
    let initial_context = build_initial_context(&session, &turn_context).await;
    session
        .replace_history(
            [initial_context.clone(), vec![user_message("turn 1 user")]].concat(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    session
        .persist_rollout_items(
            &initial_context
                .into_iter()
                .chain([user_message("turn 1 user")])
                .map(ResponseItemEnvelope::new)
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>(),
        )
        .await;

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 99).await);
    assert_rollback_success(&events, 99).await;
    assert_eq!(
        build_initial_context(&session, &turn_context).await,
        raw_history_items(&session.clone_history().await)
    );
}

#[tokio::test]
async fn thread_rollback_fails_without_persisted_thread_history() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let initial_context = build_initial_context(&session, &turn_context).await;
    session
        .replace_history(initial_context, /*reference_context_item*/ None)
        .await;
    let before = raw_history_items(&session.clone_history().await);

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_error(
        &events,
        codex_protocol::protocol::CodexErrorInfo::ThreadRollbackFailed,
    )
    .await;
    assert_eq!(before, raw_history_items(&session.clone_history().await));
}

#[tokio::test]
async fn thread_rollback_recomputes_previous_turn_settings_and_reference_context_from_replay() {
    let (session, turn_context, _rollout_path, events) = persisted_legacy_session().await;
    install_two_turn_history(&session, &turn_context).await;
    session
        .set_previous_turn_settings(Some(PreviousTurnSettings {
            model: "stale-model".to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        }))
        .await;
    {
        let mut state = session.state.lock().await;
        state.set_reference_context_item(Some(turn_context.to_turn_context_item()));
    }

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_success(&events, 1).await;
    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn thread_rollback_restores_cleared_reference_context_item_after_compaction() {
    let (mut sess, tc, rx) = make_session_and_context_with_rx().await;
    sess.state.lock().await.session_configuration.history_mode =
        codex_protocol::protocol::ThreadHistoryMode::Legacy;
    attach_thread_persistence(
        Arc::get_mut(&mut sess).expect("session should not have additional references"),
    )
    .await;

    let first_context_item = tc.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("thread settings should have turn_id");
    let compact_turn_id = "compact-turn".to_string();
    let rolled_back_turn_id = "rolled-back-turn".to_string();
    let compacted_history = vec![
        user_message("turn 1 user"),
        user_message("summary after compaction"),
    ];
    let first_window_id = Uuid::now_v7();
    let previous_window_id = Uuid::now_v7();
    let compacted_window_id = Uuid::now_v7();

    sess.persist_rollout_items(&[
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            client_id: None,
            message: "turn 1 user".to_string(),
            images: None,
            local_images: Vec::new(),
            text_elements: Vec::new(),
            ..Default::default()
        })),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(user_message("turn 1 user").into()),
        RolloutItem::ResponseItem(assistant_message("turn 1 assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: first_turn_id,
            started_at: None,
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: compact_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: "summary after compaction".to_string(),
            replacement_history: Some(
                compacted_history
                    .iter()
                    .cloned()
                    .map(ResponseItemEnvelope::new)
                    .collect(),
            ),
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: Some(7),
            first_window_id: Some(first_window_id.to_string()),
            previous_window_id: Some(previous_window_id.to_string()),
            window_id: Some(compacted_window_id.to_string()),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: compact_turn_id,
            started_at: None,
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            client_id: None,
            message: "turn 2 user".to_string(),
            images: None,
            local_images: Vec::new(),
            text_elements: Vec::new(),
            ..Default::default()
        })),
        RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some(rolled_back_turn_id.clone()),
            model: "rolled-back-model".to_string(),
            comp_hash: None,
            ..first_context_item.clone()
        }),
        RolloutItem::ResponseItem(user_message("turn 2 user").into()),
        RolloutItem::ResponseItem(assistant_message("turn 2 assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: rolled_back_turn_id,
            started_at: None,
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ])
    .await;
    sess.replace_history(
        vec![assistant_message("stale history")],
        Some(first_context_item),
    )
    .await;
    {
        let mut state = sess.state.lock().await;
        state.restore_auto_compact_window(
            /*window_number*/ 99,
            AutoCompactWindowIds {
                first_window_id: Uuid::now_v7(),
                previous_window_id: Some(Uuid::now_v7()),
                window_id: Uuid::now_v7(),
            },
        );
    }

    assert!(
        !crate::session::rollback::thread_rollback(
            &sess,
            "sub-1".to_string(),
            /*num_turns*/ 1
        )
        .await
    );
    assert_rollback_success(&rx, 1).await;

    assert_eq!(
        raw_history_items(&sess.clone_history().await),
        compacted_history
    );
    assert!(sess.reference_context_item().await.is_none());
    assert_eq!(
        sess.state.lock().await.auto_compact_window_ids(),
        AutoCompactWindowIds {
            first_window_id,
            previous_window_id: Some(previous_window_id),
            window_id: compacted_window_id,
        }
    );
    assert!(sess.current_window_id().await.ends_with(":7"));
}

#[tokio::test]
async fn thread_rollback_persists_marker_and_replays_cumulatively() {
    let (session, turn_context, rollout_path, events) = persisted_legacy_session().await;
    install_two_turn_history(&session, &turn_context).await;

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_success(&events, 1).await;
    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_success(&events, 1).await;

    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read rollout history")
    else {
        panic!("expected resumed rollout history");
    };
    assert_eq!(
        resumed
            .history
            .iter()
            .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
            .count(),
        2
    );
}

#[tokio::test]
async fn thread_rollback_persists_required_repairs_around_the_rollback_marker() {
    let (session, _turn_context, rollout_path, events) = persisted_legacy_session().await;
    let legacy_image = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,legacy".to_string(),
            },
            detail: None,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .persist_rollout_items(&[
            RolloutItem::Compacted(CompactedItem {
                message: "legacy checkpoint".to_string(),
                replacement_history: Some(vec![ResponseItemEnvelope::new(legacy_image)]),
                window_number: Some(1),
                ..Default::default()
            }),
            RolloutItem::ResponseItem(user_message("roll this turn back").into()),
            RolloutItem::ResponseItem(assistant_message("rolled-back reply").into()),
        ])
        .await;

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_success(&events, 1).await;

    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read rollout history")
    else {
        panic!("expected resumed rollout history");
    };
    let marker_index = resumed
        .history
        .iter()
        .position(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
        .expect("rollback marker");
    let repair = resumed.history[marker_index + 1..]
        .iter()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) if compacted.replacement_history_media_repair => {
                Some(compacted)
            }
            _ => None,
        })
        .expect("required media repair after rollback marker");
    assert!(repair.replacement_history.as_ref().is_some_and(|history| {
        history.iter().all(|item| {
            !matches!(
                &item.item,
                ResponseItem::Message { content, .. }
                    if content
                        .iter()
                        .any(|item| matches!(item, ContentItem::InputImage { .. }))
            )
        })
    }));
}

#[tokio::test]
async fn thread_rollback_commits_canonical_history_before_installing_live_state() {
    for failure in [
        codex_thread_store::InMemoryThreadStoreFailure::ThreadRollbackAppend,
        codex_thread_store::InMemoryThreadStoreFailure::ThreadRollbackFlush,
    ] {
        let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
        session
            .state
            .lock()
            .await
            .session_configuration
            .history_mode = codex_protocol::protocol::ThreadHistoryMode::Legacy;
        let store = attach_in_memory_thread_store(
            Arc::get_mut(&mut session).expect("session should not have additional references"),
        )
        .await;
        install_two_turn_history(&session, &turn_context).await;
        let before = raw_history_items(&session.clone_history().await);
        store.fail_next_operation(failure).await;

        let reload_required =
            crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await;
        assert!(reload_required);
        assert_rollback_error(
            &events,
            codex_protocol::protocol::CodexErrorInfo::ThreadRollbackCommitUnknown,
        )
        .await;
        assert_eq!(before, raw_history_items(&session.clone_history().await));
    }
}

#[tokio::test]
async fn thread_rollback_fails_when_turn_in_progress() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let initial_context = build_initial_context(&session, &turn_context).await;
    session
        .replace_history(initial_context, /*reference_context_item*/ None)
        .await;
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let before = raw_history_items(&session.clone_history().await);

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 1).await);
    assert_rollback_error(
        &events,
        codex_protocol::protocol::CodexErrorInfo::ThreadRollbackFailed,
    )
    .await;
    assert_eq!(before, raw_history_items(&session.clone_history().await));
}

#[tokio::test]
async fn thread_rollback_fails_when_num_turns_is_zero() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let initial_context = build_initial_context(&session, &turn_context).await;
    session
        .replace_history(initial_context, /*reference_context_item*/ None)
        .await;
    let before = raw_history_items(&session.clone_history().await);

    assert!(!crate::session::rollback::thread_rollback(&session, "sub-1".into(), 0).await);
    assert_rollback_error(
        &events,
        codex_protocol::protocol::CodexErrorInfo::ThreadRollbackFailed,
    )
    .await;
    assert_eq!(before, raw_history_items(&session.clone_history().await));
}

#[tokio::test]
async fn rollback_waits_for_terminal_publication_before_checking_active_turn() {
    let (session, context, _path, events) = persisted_legacy_session().await;
    install_two_turn_history(&session, &context).await;
    let terminal = session.reserve_history_publication().await;
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let mut rollback = Box::pin(crate::session::rollback::thread_rollback(
        &session,
        "sub-1".to_string(),
        /*num_turns*/ 1,
    ));
    assert!(futures::poll!(rollback.as_mut()).is_pending());
    *session.active_turn.lock().await = None;
    drop(terminal);
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), rollback)
            .await
            .expect("rollback should proceed after terminal publication")
    );
    assert_rollback_success(&events, 1).await;
}

#[tokio::test]
async fn rollback_queued_behind_failed_terminal_publication_requires_reload_without_marker() {
    let (session, context, path, events) = persisted_legacy_session().await;
    install_two_turn_history(&session, &context).await;
    session
        .flush_rollout()
        .await
        .expect("persist initial history");
    let terminal = session.reserve_history_publication().await;
    let mut rollback = Box::pin(crate::session::rollback::thread_rollback(
        &session,
        "sub-1".to_string(),
        /*num_turns*/ 1,
    ));
    assert!(futures::poll!(rollback.as_mut()).is_pending());
    session.quarantine_history("terminal flush failed".to_string());
    drop(terminal);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), rollback)
            .await
            .expect("rollback should report terminal failure")
    );
    assert_rollback_error(
        &events,
        codex_protocol::protocol::CodexErrorInfo::ThreadRollbackCommitUnknown,
    )
    .await;
    assert!(session.submission_admission.requires_reload());
    let history = RolloutRecorder::get_rollout_history(&path)
        .await
        .expect("canonical history");
    assert!(
        !history
            .get_rollout_items()
            .iter()
            .any(|item| { matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))) })
    );
}
