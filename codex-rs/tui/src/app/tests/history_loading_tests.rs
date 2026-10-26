//! Input stays live while older pages wait for dependent turn metadata.

use super::pagination_completion_tests::completed_history_app;
use super::session_lifecycle_requests::BlockedOlderTurnMetadata;
use super::session_lifecycle_requests::HistoryCapabilities;
use super::session_lifecycle_requests::RealtimeRequestBehavior;
use super::session_lifecycle_requests::RecordingAppServer;
use super::session_lifecycle_requests::RecordingRequestBehavior;
use super::session_lifecycle_requests::start_recording_app_server_with_realtime_speech;
use super::*;
use crate::pager_overlay::TranscriptHistoryState;
use color_eyre::eyre::Report;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;
use pretty_assertions::assert_eq;
use ratatui::layout::Size;
use tokio::sync::oneshot;

struct BlockedHistory {
    app: Box<App>,
    _home: tempfile::TempDir,
    thread_id: ThreadId,
    backend: RecordingAppServer,
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

async fn blocked_history() -> Result<BlockedHistory> {
    let titles = (0..20)
        .map(|index| format!("Turn {index}"))
        .collect::<Vec<_>>();
    let names = titles.iter().map(String::as_str).collect::<Vec<_>>();
    let (mut app, home, thread_id) = completed_history_app(&names).await?;
    app.config.cwd = AbsolutePathBuf::from_absolute_path(home.path())?;
    app.local_settings.codex_home = app.config.codex_home.clone();
    let (arm, arm_rx) = oneshot::channel();
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let mut behavior: RecordingRequestBehavior = RealtimeRequestBehavior::Forward.into();
    behavior.blocked_older_turn_metadata = Some(BlockedOlderTurnMetadata {
        arm: arm_rx,
        entered: entered_tx,
        release: release_rx,
    });
    let (mut server, requests, proxy) = start_recording_app_server_with_realtime_speech(
        &app.config,
        HistoryCapabilities::Current,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        behavior,
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await?;
    let started = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        server.resume_thread(
            &app.local_settings,
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        ),
    )
    .await
    .wrap_err("initial history resume timed out")??;
    app.transcript_cells = crate::thread_transcript::thread_items_to_transcript_cells(
        Some(thread_id),
        &app.config.cwd,
        started.turns.iter().flat_map(|turn| turn.items.clone()),
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.scrollback_has_older_history = server.has_older_history(thread_id);
    // Initial hydration also sends opaque turn cursors. Stall only the later page load.
    arm.send(())
        .map_err(|_| eyre!("metadata gate closed before it was armed"))?;
    Ok(BlockedHistory {
        app,
        _home: home,
        thread_id,
        backend: (server, requests, proxy),
        entered,
        release,
    })
}

#[tokio::test]
async fn stalled_turn_metadata_keeps_input_live_and_can_complete_or_time_out() -> Result<()> {
    for expire in [false, true] {
        let BlockedHistory {
            mut app,
            _home,
            thread_id,
            backend: (mut server, _requests, proxy),
            entered,
            release,
        } = blocked_history().await?;
        let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        app.app_event_tx = crate::app_event_sender::AppEventSender::new(event_tx);
        app.transcript_view.history = TranscriptHistoryState::LoadingOlder;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(/*owned*/ true)?;
        let size = Size::new(/*width*/ 80, /*height*/ 12);
        tui.screen_size_for_event(&TuiEvent::Resize(size))?;
        app.render_owned_transcript(&mut tui, size)?;
        assert!(app.request_older_history_page(&mut server, thread_id));
        // Gate on the dependent RPC, not admission of the initial item-page request.
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), entered).await??;
        // A cursor key flushes the first character retained by paste-burst detection.
        for key in [KeyCode::Char('x'), KeyCode::Left] {
            tokio::time::timeout(
                Duration::from_secs(/*secs*/ 5),
                app.handle_tui_event(&mut tui, &mut server, TuiEvent::Key(key.into())),
            )
            .await??;
        }
        assert_eq!(app.chat_widget.composer_text_with_pending(), "x");
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        )
        .await?;
        assert_eq!(app.chat_widget.composer_text_with_pending(), "");
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, AppEvent::OlderThreadHistoryLoaded { .. }));
        }
        let release_after_timeout = if expire {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(/*secs*/ 31)).await;
            Some(release)
        } else {
            release
                .send(())
                .map_err(|_| eyre!("metadata gate closed"))?;
            None
        };
        let loaded = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
            loop {
                let event = events.recv().await.ok_or_else(|| eyre!("events closed"))?;
                if matches!(event, AppEvent::OlderThreadHistoryLoaded { .. }) {
                    break Ok::<_, Report>(event);
                }
            }
        })
        .await??;
        if let Some(release) = release_after_timeout {
            tokio::time::resume();
            release
                .send(())
                .map_err(|_| eyre!("metadata gate closed"))?;
        }
        let AppEvent::OlderThreadHistoryLoaded {
            request, result, ..
        } = &loaded
        else {
            unreachable!()
        };
        let request = request.clone();
        assert_eq!(result.is_err(), expire);
        Box::pin(app.handle_event(&mut tui, &mut server, loaded)).await?;
        if expire {
            assert_eq!(app.transcript_view.history, TranscriptHistoryState::Failed);
            let retry = server
                .begin_older_history_page(thread_id)
                .expect("retryable page");
            assert_eq!(retry.request.cursor, request.cursor);
            assert_ne!(retry.request, request);
            server.cancel_older_history_page(thread_id, &retry.request);
        } else {
            assert_eq!(
                app.transcript_view.history,
                TranscriptHistoryState::Complete
            );
            assert!(!server.has_older_history(thread_id));
        }
        tui.set_owned_screen(/*owned*/ false)?;
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_metadata_load_releases_admission_before_the_server_replies() -> Result<()> {
    let BlockedHistory {
        app,
        _home,
        thread_id,
        backend: (mut server, _requests, proxy),
        entered,
        release,
    } = blocked_history().await?;
    let turns = {
        let store = app.thread_event_channels[&thread_id].store.lock().await;
        let mut turns = store.turns.clone();
        for turn in &mut turns {
            turn.items.clear();
        }
        turns
    };
    let load = server
        .begin_older_history_page(thread_id)
        .expect("older page");
    let request = load.request.clone();
    let work = tokio::spawn(load.fetch(turns.clone()));
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), entered).await??;
    server.cancel_older_history_page(thread_id, &request);
    let error = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), work)
        .await??
        .expect_err("cancelled work");
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let retry = server
        .begin_older_history_page(thread_id)
        .expect("retry before reply");
    assert_eq!(retry.request.cursor, request.cursor);
    assert_ne!(retry.request, request);
    assert!(!server.is_older_history_page_pending(thread_id, &request));
    assert!(server.is_older_history_page_pending(thread_id, &retry.request));
    release
        .send(())
        .map_err(|_| eyre!("metadata gate closed"))?;
    let request = retry.request.clone();
    let page = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), retry.fetch(turns)).await??;
    let contents = server
        .apply_older_history_page(thread_id, &request, page)
        .expect("current admission");
    assert!(!contents.items.is_empty());
    assert!(!server.has_older_history(thread_id));
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
