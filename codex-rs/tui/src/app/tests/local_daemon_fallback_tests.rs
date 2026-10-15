//! Cold recovery uses durable history, not old process-local requests or queue authority.

use super::*;
use crate::app::local_daemon_fallback::EmbeddedAppServerRecovery;
use crate::app::local_daemon_fallback::LocalDaemonFallback;
use crate::app::reconnect::ReconnectPresentation;
use crate::app::reconnect::reconnect;
use crate::dynamic_tools_mcp::ThreadToolTransport;
use app_test_support::MockResponsesConfig;
use codex_arg0::Arg0DispatchPaths;
use core_test_support::responses;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Read;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

const PEER_SOCKET: &str = "CODEX_TUI_RECOVERY_TEST_SOCKET";

// Only the test-owned subprocess sees this environment variable.
#[test]
fn daemon_peer_child() -> Result<()> {
    let Some(socket) = std::env::var_os(PEER_SOCKET) else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut stream = codex_uds::UnixStream::connect(socket).await?;
            stream.write_all(b"ready").await
        })?;
    std::io::stdin().read_exact(&mut [0])?;
    Ok(())
}

struct PeerChild(std::process::Child);

impl PeerChild {
    async fn exit(&mut self) -> Result<()> {
        self.0
            .stdin
            .take()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing child stdin"))?
            .write_all(b"x")?;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                if let Some(status) = self.0.try_wait()? {
                    assert!(status.success());
                    return Ok::<_, color_eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
        })
        .await??;
        Ok(())
    }
}

impl Drop for PeerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn monitored_peer() -> Result<(PeerChild, codex_uds::PeerProcess)> {
    let directory = tempdir()?;
    let socket = directory.path().join("peer.sock");
    let mut listener = codex_uds::UnixListener::bind(&socket).await?;
    let child = PeerChild(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "app::tests::local_daemon_fallback::daemon_peer_child",
                "--nocapture",
            ])
            .env(PEER_SOCKET, &socket)
            .stdin(Stdio::piped())
            .spawn()?,
    );
    let monitor = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        let mut stream = listener.accept().await?;
        let monitor = stream.peer_process()?;
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        assert!(!monitor.has_exited()?);
        Ok::<_, color_eyre::Report>(monitor)
    })
    .await??;
    Ok((child, monitor))
}

fn recovery_startup(app: &App) -> EmbeddedAppServerRecovery {
    EmbeddedAppServerRecovery {
        arg0_paths: Arg0DispatchPaths::default(),
        config: app.config.clone(),
        cli_kv_overrides: Vec::new(),
        loader_overrides: LoaderOverrides::without_managed_config_for_tests(),
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        feedback: codex_feedback::CodexFeedback::new(),
        log_db: None,
        environment_manager: Arc::clone(&app.environment_manager),
        network_policy: Default::default(),
    }
}

fn local_target(home: &Path) -> Result<AppServerTarget> {
    Ok(AppServerTarget::LocalDaemon {
        endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::from_absolute_path(home.join("absent.sock"))?,
        },
        // Required-daemon startup must still recover from proven death after attachment.
        allow_embedded_fallback: false,
    })
}

#[tokio::test]
async fn dead_daemon_resumes_history_keeps_input_and_accepts_a_fresh_turn() -> Result<()> {
    let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
    let home = tempdir()?;
    let cwd = tempdir()?;
    let model = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &model,
        responses::sse(vec![
            responses::ev_response_created("after-recovery"),
            responses::ev_completed("after-recovery"),
        ]),
    )
    .await;
    MockResponsesConfig::new(&model.uri()).write(home.path())?;
    app.config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut owner = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = owner.start_thread(&app.config).await?;
    let id = started.session.thread_id;
    owner
        .thread_inject_items(
            id,
            vec![serde_json::from_value(json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Saved before daemon loss"}],
            }))?],
        )
        .await?;
    app.active_thread_id = Some(id);
    app.primary_thread_id = Some(id);
    app.ensure_thread_channel(id)
        .store
        .lock()
        .await
        .set_session(started.session.clone(), started.turns);
    app.chat_widget.handle_thread_session(started.session);
    app.chat_widget
        .restore_user_message_to_composer("uncertain submission".into());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    next_user_turn_op(&mut ops);
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ true);
    app.chat_widget
        .restore_user_message_to_composer("queued before failure".into());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.chat_widget
        .restore_user_message_to_composer("keep my draft".into());

    let background = ThreadId::new();
    app.agent_navigation.upsert(
        background,
        Some("Worker".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.agent_navigation.mark_running(background);
    app.agent_navigation
        .replace_user_reply_route(id, background, /*enabled*/ true);
    app.agent_navigation.reserve_prompt_response(id, background);
    app.runtime_approval_policy_override = Some(RuntimeApprovalPolicyOverride::Explicit(
        AskForApproval::OnRequest,
    ));
    app.runtime_approvals_reviewer_override = Some(ApprovalsReviewer::User);
    app.runtime_permission_profile_override =
        Some(RuntimePermissionProfileOverride::from_config(&app.config));
    app.pending_server_profiles.insert(
        background,
        PermissionProfileSelection {
            profile_id: "dead-server-only".into(),
            approval_policy: None,
            approvals_reviewer: None,
            display_label: "dead-server-only".into(),
        },
    );
    app.ensure_thread_channel(background)
        .store
        .lock()
        .await
        .set_session(
            test_thread_session(background, cwd.path().to_path_buf()),
            vec![test_turn("old-turn", TurnStatus::InProgress, Vec::new())],
        );
    let (mut child, peer) = monitored_peer().await?;
    owner.shutdown().await?;
    child.exit().await?;
    assert!(peer.has_exited()?);
    app.app_server_target = local_target(home.path())?;
    assert!(app.begin_reconnect());
    let old_sender = app.app_event_tx.clone();
    let mut old_session = crate::start_embedded_app_server_for_picker(&app.config).await?;
    old_session.remember_task_tool_thread(id);
    let connected = reconnect(
        app.app_server_target.clone(),
        app.config.clone(),
        app.local_settings.clone(),
        Some(id),
        /*remote_cwd*/ None,
        ThreadToolTransport::Dynamic,
        ReconnectPresentation::Conversation,
        Some(LocalDaemonFallback {
            startup: recovery_startup(&app),
            peer,
        }),
    )
    .await?;
    assert!(connected.session.uses_embedded_app_server());
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.finish_reconnect(
        &mut tui,
        &mut old_session,
        &mut events,
        connected,
        CODEX_CLI_VERSION,
    )
    .await?;
    assert_eq!(app.app_server_target, AppServerTarget::Embedded);
    assert_eq!(app.current_displayed_thread_id(), Some(id));
    assert!(!app.is_offline());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "keep my draft"
    );
    assert!(app.chat_widget.has_queued_follow_up_messages());
    assert!(!app.chat_widget.maybe_send_next_queued_input());
    assert!(matches!(
        old_session.thread_tool_transport(),
        ThreadToolTransport::Disabled
    ));
    assert_eq!(app.agent_navigation.reply_route(id, background), None);
    assert_eq!(
        app.agent_navigation.reserved_prompt_source(background),
        None
    );
    assert!(app.pending_server_profiles.is_empty());
    assert!(app.runtime_approval_policy_override.is_none());
    assert!(app.runtime_approvals_reviewer_override.is_none());
    assert!(app.runtime_permission_profile_override.is_none());
    {
        let background_store = app.thread_event_channels[&background].store.lock().await;
        assert_eq!(
            background_store.turns,
            vec![test_turn("old-turn", TurnStatus::Interrupted, Vec::new())]
        );
        assert!(background_store.buffer.is_empty());
    }
    old_sender.send(AppEvent::CodexOp(AppCommand::Interrupt));
    let mut recovery_notice = None;
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event,
            AppEvent::CodexOp(AppCommand::UserTurn { .. } | AppCommand::Interrupt)
        ));
        if let AppEvent::InsertHistoryCell(cell) = &event {
            let text = lines_to_single_string(&cell.display_lines(/*width*/ 240));
            if text.contains("Local background server exited") {
                recovery_notice = Some(text);
            }
        }
    }
    assert_snapshot!(
        recovery_notice.ok_or_else(|| color_eyre::eyre::eyre!("missing recovery notice"))?,
        @"• Local background server exited; recovered this session and its agents in this TUI. Resumed 0 interrupted turns. Pending inputs were not resent."
    );
    let history = old_session.thread_read(id, /*include_turns*/ true).await?;
    let history_json = serde_json::to_string(&history.turns)?;
    assert!(!history_json.contains("uncertain submission"));
    assert!(!history_json.contains("queued before failure"));
    assert!(response.requests().is_empty());

    // A deliberately new prompt is usable without replaying the paused queue.
    app.chat_widget.apply_external_edit("fresh input".into());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let operation = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::CodexOp(op @ AppCommand::UserTurn { .. }) => Some(op),
            _ => None,
        })
        .ok_or_else(|| color_eyre::eyre::eyre!("fresh input was not submitted"))?;
    assert!(
        app.try_submit_active_thread_op_via_app_server(&mut old_session, id, &operation)
            .await?
    );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            if !response.requests().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    let request = response.single_request().body_json();
    let request = serde_json::to_string(&request)?;
    // Raw injected items belong to model history, not the paginated transcript.
    assert!(request.contains("Saved before daemon loss"));
    assert!(request.contains("fresh input"));
    assert!(!request.contains("uncertain submission"));
    assert!(!request.contains("queued before failure"));
    old_session.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn dead_peer_never_overrides_a_live_writer() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let mut owner = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = owner.start_thread(&app.config).await?;
    let id = started.session.thread_id;
    owner
        .thread_inject_items(id, vec![App::side_boundary_prompt_item()])
        .await?;
    let (mut child, peer) = monitored_peer().await?;
    child.exit().await?;
    app.chat_widget
        .restore_user_message_to_composer("writer-conflict draft".into());
    app.app_server_target = local_target(app.config.codex_home.as_path())?;
    assert!(app.begin_reconnect());
    let result = reconnect(
        app.app_server_target.clone(),
        app.config.clone(),
        app.local_settings.clone(),
        Some(id),
        /*remote_cwd*/ None,
        ThreadToolTransport::Dynamic,
        ReconnectPresentation::Conversation,
        Some(LocalDaemonFallback {
            startup: recovery_startup(&app),
            peer,
        }),
    )
    .await;
    let error = match result {
        Ok(connected) => {
            connected.session.shutdown().await?;
            color_eyre::eyre::bail!("recovery took a live writer's thread");
        }
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("writer"), "{error:#}");
    assert!(app.is_offline());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "writer-conflict draft"
    );
    owner.thread_set_name(id, "Still owned".into()).await?;
    assert_eq!(
        owner
            .thread_read(id, /*include_turns*/ false)
            .await?
            .name
            .as_deref(),
        Some("Still owned")
    );
    owner.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn live_peer_missing_evidence_remote_and_unknown_thread_stay_reconnect_only() -> Result<()> {
    let (app, _events, _ops) = make_test_app_with_channels().await;
    let (mut child, peer) = monitored_peer().await?;
    let local = local_target(app.config.codex_home.as_path())?;
    let remote = match local.clone() {
        AppServerTarget::LocalDaemon { endpoint, .. } => AppServerTarget::Remote { endpoint },
        _ => unreachable!(),
    };
    let id = ThreadId::new();
    for (target, evidence, thread_id) in [
        (local.clone(), Some(peer.clone()), Some(id)),
        (local.clone(), None, Some(id)),
        (remote, Some(peer.clone()), Some(id)),
        (local.clone(), Some(peer.clone()), None),
    ] {
        if target != local {
            child.exit().await?;
            assert!(peer.has_exited()?);
        }
        tokio::time::pause();
        let result = reconnect(
            target,
            app.config.clone(),
            app.local_settings.clone(),
            thread_id,
            /*remote_cwd*/ None,
            ThreadToolTransport::Dynamic,
            ReconnectPresentation::Conversation,
            evidence.map(|peer| LocalDaemonFallback {
                startup: recovery_startup(&app),
                peer,
            }),
        )
        .await;
        tokio::time::resume();
        let error = match result {
            Ok(connected) => {
                connected.session.shutdown().await?;
                color_eyre::eyre::bail!("reconnected without a live endpoint or eligible fallback");
            }
            Err(error) => error,
        };
        let expected = if thread_id.is_some() {
            "Server connection could not be restored"
        } else {
            "The initial thread may have been created, but its ID was not received. Nothing was retried. Your prompt is editable; inspect your tasks before relaunching."
        };
        assert_eq!(error.to_string(), expected);
    }
    Ok(())
}
