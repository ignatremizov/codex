use super::*;
use crate::codex_thread::BackgroundTerminalInfo;
use crate::environment_selection::TurnEnvironmentState;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecRequest;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_current_turn_settings_for_test;
use crate::session::tests::update_selected_settings_for_test;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::start_streaming_output;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use codex_protocol::protocol::TerminalWaitEvent;
use codex_protocol::protocol::TerminalWaitMode;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::test_env as remote_test_env;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;

pub(super) async fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, turn) = make_session_and_context().await;
    (Arc::new(session), Arc::new(turn))
}

pub(super) async fn exec_command(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    exec_command_with_tty(
        session,
        turn,
        cmd,
        yield_time_ms,
        workdir,
        /*tty*/ true,
    )
    .await
}

fn shell_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn test_exec_request(
    turn: &TurnContext,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    env: HashMap<String, String>,
) -> ExecRequest {
    let permission_profile = turn.permission_profile();
    let network = None;
    let arg0 = None;
    ExecRequest::new(
        command,
        cwd,
        env,
        network,
        /*network_environment_id*/ None,
        ExecExpiration::DefaultTimeout,
        ExecCapturePolicy::ShellTool,
        SandboxType::None,
        turn.config
            .effective_workspace_roots()
            .iter()
            .map(PathUri::to_abs_path)
            .collect::<std::io::Result<Vec<_>>>()
            .expect("test workspace roots are host-native"),
        turn.windows_sandbox_level,
        permission_profile,
        arg0,
    )
}

async fn exec_command_with_tty(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
    tty: bool,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    #[allow(deprecated)]
    let cwd = workdir
        .as_ref()
        .map_or_else(|| turn.cwd.clone(), |workdir| turn.cwd.join(workdir));
    let command = vec!["bash".to_string(), "-lc".to_string(), cmd.to_string()];
    let request = test_exec_request(turn, command.clone(), cwd.clone(), shell_env());

    let process = manager
        .open_session_with_prepared_exec_env(
            process_id,
            &request,
            /*tool_ctx*/ None,
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            /*network_policy_decider*/ None,
            tty,
            Box::new(NoopSpawnLifecycle),
            /*shell_snapshot_file*/ None,
            turn.initial_environments
                .primary()
                .expect("turn environment")
                .environment
                .as_ref(),
        )
        .await?;
    let context = UnifiedExecContext::new(
        Arc::clone(session),
        crate::session::step_context::StepContext::for_test(Arc::clone(turn)),
        tokio_util::sync::CancellationToken::new(),
        "call".to_string(),
    );
    start_streaming_output(&process, &context);
    let started_at = Instant::now();
    let process_started_alive = !process.has_exited() && process.exit_code().is_none();
    if process_started_alive {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            plugin_metrics_sidecar: None,
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: cmd.to_string(),
            tty,
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            permissions: TerminalPermissions::for_launch(
                turn.initial_environments
                    .primary()
                    .expect("turn environment"),
                turn,
                TerminalSandboxSource::Native,
                SandboxPermissions::UseDefault,
                /*additional_permissions*/ None,
                /*internal_permissions*/ None,
            ),
            network_approval: None,
            session: Arc::downgrade(session),
            last_used: started_at,
        };
        manager
            .process_store
            .lock()
            .await
            .processes
            .insert(process_id, entry);
    }

    let deadline = started_at.checked_add(Duration::from_millis(yield_time_ms));
    let collected_output = super::output_collection::collect_output_until_deadline(
        process.output_handles(),
        Some(session.subscribe_elicitation_pause_state()),
        deadline,
        None,
    )
    .await
    .collected;
    let wall_time = Instant::now().saturating_duration_since(started_at);
    let original_token_count = usize::try_from(approx_tokens_from_byte_count(
        collected_output.total_bytes(),
    ))
    .unwrap_or(usize::MAX);
    let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
    let collected = collected_output.to_bytes_with_omission_marker();
    let has_exited = process.has_exited();
    let exit_code = process.exit_code();
    let response_process_id = if process_started_alive && !has_exited {
        Some(process_id)
    } else {
        manager.release_process_id(process_id).await;
        None
    };
    if response_process_id.is_some()
        && let Some(entry) = manager
            .process_store
            .lock()
            .await
            .processes
            .get_mut(&process_id)
    {
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    Ok(ExecCommandToolOutput {
        event_call_id: context.call_id,
        chunk_id: generate_chunk_id(),
        wall_time,
        raw_output: collected,
        truncation_policy: turn.model_info().truncation_policy.into(),
        max_output_tokens: None,
        process_id: response_process_id,
        exit_code,
        original_token_count: Some(original_token_count),
        output_omitted_bytes,
        hook_command: Some(cmd.to_string()),
    })
}

#[derive(Debug)]
struct TestSpawnLifecycle {
    inherited_fds: Vec<i32>,
}

impl SpawnLifecycle for TestSpawnLifecycle {
    fn inherited_fds(&self) -> Vec<i32> {
        self.inherited_fds.clone()
    }
}

struct BlockingTerminateExecProcess {
    process_id: ProcessId,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
    write_started: Option<watch::Sender<bool>>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
    wake_tx: watch::Sender<u64>,
}

impl BlockingTerminateExecProcess {
    async fn read(&self) -> Result<ReadResponse, codex_exec_server::ExecServerError> {
        Ok(ReadResponse {
            chunks: Vec::new(),
            next_seq: 1,
            exited: false,
            exit_code: None,
            closed: false,
            failure: None,
            sandbox_denied: false,
        })
    }

    async fn write(
        &self,
        chunk: Vec<u8>,
    ) -> Result<WriteResponse, codex_exec_server::ExecServerError> {
        if let Some(write_started) = self.write_started.as_ref() {
            let _ = write_started.send(true);
        }
        if let Some(write_tx) = self.write_tx.as_ref() {
            let _ = write_tx.send(chunk).await;
        }
        Ok(WriteResponse {
            status: WriteStatus::Accepted,
        })
    }

    async fn terminate(&self) -> Result<(), codex_exec_server::ExecServerError> {
        let _ = self.terminate_started.send(true);
        self.allow_terminate.notified().await;
        Ok(())
    }
}

impl ExecProcess for BlockingTerminateExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(BlockingTerminateExecProcess::read(self))
    }

    fn write(&self, chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(BlockingTerminateExecProcess::write(self, chunk))
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(BlockingTerminateExecProcess::terminate(self))
    }
}

async fn blocking_terminate_unified_process(
    process_id: i32,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
) -> anyhow::Result<Arc<UnifiedExecProcess>> {
    let (wake_tx, _wake_rx) = watch::channel(0);
    Ok(Arc::new(
        UnifiedExecProcess::from_exec_server_started(StartedExecProcess {
            process: Arc::new(BlockingTerminateExecProcess {
                process_id: process_id.to_string().into(),
                terminate_started,
                allow_terminate,
                write_started: None,
                write_tx: None,
                wake_tx,
            }),
            sandbox_type: Some(codex_sandboxing::SandboxType::None),
        })
        .await?,
    ))
}

async fn write_stdin(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    process_id: i32,
    input: &str,
    yield_time_ms: u64,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    write_stdin_with_options(
        session,
        turn,
        WriteStdinOptions {
            process_id,
            input,
            yield_time_ms,
            wait_until_exit: false,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            user_input_wait: None,
            interaction_id: None,
        },
    )
    .await
}

pub(super) struct WriteStdinOptions<'a> {
    pub process_id: i32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub wait_until_exit: bool,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub user_input_wait: Option<UserInputWait>,
    pub interaction_id: Option<&'a str>,
}

pub(super) async fn write_stdin_with_options(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    options: WriteStdinOptions<'_>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    let WriteStdinOptions {
        process_id,
        input,
        yield_time_ms,
        wait_until_exit,
        cancellation_token,
        user_input_wait,
        interaction_id,
    } = options;
    let interaction_event = interaction_id.map(|interaction_id| WriteStdinInteractionEvent {
        session,
        turn,
        interaction_id,
    });
    session
        .services
        .unified_exec_manager
        .write_stdin(
            &UnifiedExecContext::new(
                Arc::clone(session),
                crate::session::step_context::StepContext::for_test_with_current_settings(
                    Arc::clone(turn),
                ),
                cancellation_token,
                "write".to_string(),
            ),
            WriteStdinRequest {
                process_id,
                input,
                yield_time_ms,
                wait_until_exit,
                max_output_tokens: None,
                truncation_policy: TruncationPolicy::Tokens(10_000),
                interaction_event,
                user_input_wait,
            },
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_persists_across_requests() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process_id");
    assert_eq!(
        session.list_background_terminals().await,
        vec![BackgroundTerminalInfo {
            item_id: "call".to_string(),
            process_id: process_id.to_string(),
            command: "bash -i".to_string(),
            cwd: cwd.into(),
            user_shell_response_handling: None,
        }]
    );

    write_stdin(
        &session,
        &turn,
        process_id,
        "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        &turn,
        process_id,
        "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    assert!(
        out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "expected environment variable output"
    );

    assert!(session.terminate_background_terminal(process_id).await);
    assert!(!session.terminate_background_terminal(process_id).await);
    assert!(session.list_background_terminals().await.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_unified_exec_sessions() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;

    let shell_a = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let session_a = shell_a.process_id.expect("expected process id");

    write_stdin(
        &session,
        &turn,
        session_a,
        "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = exec_command(
        &session,
        &turn,
        "echo $CODEX_INTERACTIVE_SHELL_VAR",
        /*yield_time_ms*/ 2_500,
        /*workdir*/ None,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        out_2.process_id.is_none(),
        "short command should not report a process id if it exits quickly"
    );
    assert!(
        !out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "short command should run in a fresh shell"
    );

    let out_3 = write_stdin(
        &session,
        &turn,
        shell_a.process_id.expect("expected process id"),
        "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    assert!(
        out_3
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "session should preserve state"
    );

    Ok(())
}

#[tokio::test]
async fn unified_exec_timeouts() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    const TEST_VAR_VALUE: &str = "unified_exec_var_123";

    let (session, turn) = test_session_and_turn().await;

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process id");

    write_stdin(
        &session,
        &turn,
        process_id,
        format!("export CODEX_INTERACTIVE_SHELL_VAR={TEST_VAR_VALUE}\n").as_str(),
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        &turn,
        process_id,
        "sleep 5 && echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 10,
    )
    .await?;
    assert!(
        !out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "timeout too short should yield incomplete output"
    );

    tokio::time::sleep(Duration::from_secs(7)).await;

    let out_3 = write_stdin(&session, &turn, process_id, "", /*yield_time_ms*/ 100).await?;

    assert!(
        out_3
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "subsequent poll should retrieve output"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_pause_blocks_yield_timeout() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;
    let elicitation = session.services.elicitations.register();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(elicitation);
    });

    let started = tokio::time::Instant::now();
    let response = exec_command(
        &session,
        &turn,
        "sleep 1 && echo unified-exec-done",
        /*yield_time_ms*/ 250,
        /*workdir*/ None,
    )
    .await?;

    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "pause should block the unified exec yield timeout"
    );
    assert!(
        response
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("unified-exec-done"),
        "exec_command should wait for output after the pause lifts"
    );
    assert!(
        response.process_id.is_none(),
        "completed command should not leave a background process"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_completed_process_returns_unknown_process() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process id");

    write_stdin(
        &session, &turn, process_id, "exit\n", /*yield_time_ms*/ 2_500,
    )
    .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let err = write_stdin(&session, &turn, process_id, "", /*yield_time_ms*/ 100)
        .await
        .expect_err("expected unknown process error");

    match err {
        UnifiedExecError::UnknownProcessId { process_id: err_id } => {
            assert_eq!(err_id, process_id, "process id should match request");
        }
        other => panic!("expected UnknownProcessId, got {other:?}"),
    }

    assert!(
        session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await
            .processes
            .is_empty()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminating_initial_exec_command_rechecks_initial_response_state() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, mut terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process,
            plugin_metrics_sidecar: None,
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: "sleep 60".to_string(),
            tty: true,
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            permissions: TerminalPermissions::for_launch(
                turn.initial_environments
                    .primary()
                    .expect("turn environment"),
                &turn,
                TerminalSandboxSource::Native,
                SandboxPermissions::UseDefault,
                /*additional_permissions*/ None,
                /*internal_permissions*/ None,
            ),
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
        },
    );

    let terminate_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.terminate_background_terminal(process_id).await }
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        terminate_started_rx.wait_for(|started| *started),
    )
    .await
    .expect("terminate should start")
    .expect("terminate signal sender should stay open");

    {
        let mut store = manager.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .expect("process should remain stored until initial response returns");
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    allow_terminate.notify_waiters();
    let terminated = tokio::time::timeout(Duration::from_secs(2), terminate_task)
        .await
        .expect("terminate should finish")
        .expect("terminate task should not panic");
    assert!(terminated);
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_stdin_poll_can_be_resumed_and_observe_process_exit() -> anyhow::Result<()> {
    for failure in [None, Some("executor stream failed")] {
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let manager = &session.services.unified_exec_manager;
        let process_id = manager.allocate_process_id().await;
        let (terminate_started_tx, _terminate_started_rx) = watch::channel(false);
        let allow_terminate = Arc::new(Notify::new());
        let process = blocking_terminate_unified_process(
            process_id,
            terminate_started_tx,
            Arc::clone(&allow_terminate),
        )
        .await?;
        let context = UnifiedExecContext::new(
            Arc::clone(&session),
            crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
            CancellationToken::new(),
            "call".to_string(),
        );
        start_streaming_output(&process, &context);
        #[allow(deprecated)]
        let cwd = turn.cwd.clone();
        let last_used = Instant::now() - Duration::from_secs(1);
        manager.process_store.lock().await.processes.insert(
            process_id,
            ProcessEntry {
                process: Arc::clone(&process),
                plugin_metrics_sidecar: None,
                call_id: "call".to_string(),
                process_id,
                cwd: cwd.into(),
                initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                hook_command: "sleep 60".to_string(),
                tty: true,
                environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
                permissions: TerminalPermissions::for_launch(
                    turn.initial_environments
                        .primary()
                        .expect("turn environment"),
                    &turn,
                    TerminalSandboxSource::Native,
                    SandboxPermissions::UseDefault,
                    /*additional_permissions*/ None,
                    /*internal_permissions*/ None,
                ),
                network_approval: None,
                session: Arc::downgrade(&session),
                last_used,
            },
        );

        let spawn_poll = || {
            tokio::spawn({
                let session = Arc::clone(&session);
                let turn = Arc::clone(&turn);
                async move {
                    session.services.unified_exec_manager.write_stdin(
                &UnifiedExecContext::new(
                    Arc::clone(&session),
                    crate::session::step_context::StepContext::for_test_with_current_settings(
                        Arc::clone(&turn),
                    ),
                    tokio_util::sync::CancellationToken::new(),
                    "poll-call".to_string(),
                ),
                WriteStdinRequest {
                    process_id,
                    input: "",
                    yield_time_ms: u64::MAX,
                    wait_until_exit: false,
                    max_output_tokens: None,
                    truncation_policy: TruncationPolicy::Tokens(10_000),
                    interaction_event: Some(WriteStdinInteractionEvent {
                        session: &session,
                        turn: &turn,
                        interaction_id: "poll-call",
                    }),
                    user_input_wait: None,
                },
            ).await
                }
            })
        };
        let next_interaction = || async {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let event = events.recv().await.expect("event channel stays open");
                    if let codex_protocol::protocol::EventMsg::TerminalInteraction(interaction) =
                        event.msg
                    {
                        break interaction;
                    }
                }
            })
            .await
            .expect("poll lifecycle event should arrive")
        };
        let polls_started = Instant::now();
        let poll_task = spawn_poll();
        let first_begin = next_interaction().await;
        assert!(process.interaction_lock().try_lock_owned().is_err());

        poll_task.abort();
        let cancelled = tokio::time::timeout(Duration::from_secs(2), poll_task)
            .await
            .expect("cancelling a poll should not wait for the process to exit")
            .expect_err("the polling task should be cancelled");
        assert!(cancelled.is_cancelled());
        assert!(!process.has_exited());
        assert!(process.interaction_lock().try_lock_owned().is_ok());
        assert!(
            events.try_recv().is_err(),
            "abort must not enqueue a delayed clear"
        );

        let poll_task = spawn_poll();
        let second_begin = next_interaction().await;

        allow_terminate.notify_one();
        if let Some(message) = failure {
            process.fail_and_terminate(message.to_string());
        } else {
            manager.release_process_id(process_id).await;
            process.terminate_confirmed().await?;
        }

        let output = tokio::time::timeout(Duration::from_secs(2), poll_task)
            .await
            .expect("poll should finish")
            .expect("poll task should not panic");
        if let Some(message) = failure {
            assert!(
                output
                    .expect_err("failed stream should fail the poll")
                    .to_string()
                    .contains(message)
            );
        } else {
            assert_eq!(output?.process_id, None);
        }
        assert!(manager.process_store.lock().await.processes.is_empty());
        let clear = next_interaction().await;
        let (
            Some(TerminalWaitEvent::Started {
                started_at_ms: first_started_at_ms,
                ..
            }),
            Some(TerminalWaitEvent::Started {
                started_at_ms: second_started_at_ms,
                ..
            }),
            Some(TerminalWaitEvent::Finished { elapsed_ms, .. }),
        ) = (&first_begin.wait, &second_begin.wait, &clear.wait)
        else {
            anyhow::bail!("expected two wait starts and one wait completion");
        };
        assert!(*first_started_at_ms > 0);
        assert!(second_started_at_ms >= first_started_at_ms);
        assert!(u128::from(*elapsed_ms) <= polls_started.elapsed().as_millis());
        let expected = |wait| TerminalInteractionEvent {
            call_id: "call".to_string(),
            process_id: process_id.to_string(),
            stdin: String::new(),
            deadline_at_ms: None,
            wait: Some(wait),
        };
        let expected_events = [
            expected(TerminalWaitEvent::Started {
                interaction_id: "poll-call".to_string(),
                started_at_ms: *first_started_at_ms,
                mode: TerminalWaitMode::Timed,
            }),
            expected(TerminalWaitEvent::Started {
                interaction_id: "poll-call".to_string(),
                started_at_ms: *second_started_at_ms,
                mode: TerminalWaitMode::Timed,
            }),
            expected(TerminalWaitEvent::Finished {
                interaction_id: "poll-call".to_string(),
                elapsed_ms: *elapsed_ms,
                reason: if failure.is_some() {
                    TerminalWaitCompletionReason::Failed
                } else {
                    TerminalWaitCompletionReason::Exited
                },
            }),
        ];
        assert_eq!([first_begin, second_begin, clear], expected_events);
        assert!(
            events.try_recv().is_err(),
            "only one ordinary-result clear is emitted"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_blocked_stdin_write_releases_the_process_interaction_lock() -> anyhow::Result<()>
{
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (write_started_tx, mut write_started_rx) = watch::channel(/*init*/ false);
    let (terminate_started_tx, _terminate_started_rx) = watch::channel(/*init*/ false);
    let (wake_tx, _wake_rx) = watch::channel(/*init*/ 0);
    let (write_tx, mut write_rx) = mpsc::channel(/*buffer*/ 1);
    let allow_terminate = Arc::new(Notify::new());
    write_tx
        .send(Vec::new())
        .await
        .expect("blocked stdin channel should stay open");
    let process = Arc::new(
        UnifiedExecProcess::from_exec_server_started(StartedExecProcess {
            process: Arc::new(BlockingTerminateExecProcess {
                process_id: process_id.to_string().into(),
                terminate_started: terminate_started_tx,
                allow_terminate: Arc::clone(&allow_terminate),
                write_started: Some(write_started_tx),
                write_tx: Some(write_tx),
                wake_tx,
            }),
            sandbox_type: Some(codex_sandboxing::SandboxType::None),
        })
        .await?,
    );
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            plugin_metrics_sidecar: None,
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "sleep 60".to_string(),
            tty: true,
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            permissions: TerminalPermissions::for_launch(
                turn.environments.primary().expect("turn environment"),
                &turn,
                TerminalSandboxSource::Native,
                SandboxPermissions::UseDefault,
                /*additional_permissions*/ None,
                /*internal_permissions*/ None,
            ),
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
        },
    );

    let cancellation_token = CancellationToken::new();
    let write_cancellation_token = cancellation_token.clone();
    let write_task = tokio::spawn({
        let session = Arc::clone(&session);
        let turn = Arc::clone(&turn);
        async move {
            write_stdin_with_options(
                &session,
                &turn,
                WriteStdinOptions {
                    process_id,
                    input: "blocked input",
                    yield_time_ms: 100,
                    wait_until_exit: false,
                    cancellation_token: write_cancellation_token,
                    user_input_wait: None,
                    interaction_id: None,
                },
            )
            .await
        }
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        write_started_rx.wait_for(|started| *started),
    )
    .await
    .expect("stdin write should block")
    .expect("write-started sender should stay open");

    cancellation_token.cancel();
    let cancelled_write = tokio::time::timeout(Duration::from_secs(2), write_task)
        .await
        .expect("cancelled stdin write should release its task")
        .expect("stdin write task should not panic");
    assert!(cancelled_write.is_err());
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id),
        "cancelling a write must preserve the process"
    );
    assert_eq!(
        write_rx.recv().await,
        Some(Vec::new()),
        "the cancelled write must leave the queued stdin payload untouched"
    );
    assert!(matches!(
        write_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let followup_write = tokio::time::timeout(
        Duration::from_secs(2),
        write_stdin(
            &session, &turn, process_id, "\n", /*yield_time_ms*/ 100,
        ),
    )
    .await
    .expect("subsequent same-process write should acquire the interaction lock")?;
    assert_eq!(followup_write.process_id, Some(process_id));
    allow_terminate.notify_one();
    process.terminate_confirmed().await?;
    if let Some(output_task) = process.output_task_abort_handle() {
        output_task.abort();
    }
    manager.release_process_id(process_id).await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_process_retains_shell_snapshot_until_durable_shutdown() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = make_session_and_context().await;
    let directory = tempfile::tempdir()?;
    let cwd = AbsolutePathBuf::try_from(directory.path())?;
    let environment = Arc::new(codex_exec_server::Environment::default_for_tests());
    let shell = crate::shell::Shell {
        shell_type: crate::shell::ShellType::Bash,
        shell_path: which::which("bash")?,
    };
    let snapshot = crate::shell_snapshot::ShellSnapshot::new(
        cwd.clone(),
        session.thread_id(),
        turn.session_telemetry.clone(),
        /*state_db*/ None,
        /*credential_broker*/ None,
        /*prefer_executor_snapshots*/ false,
    )
    .build(
        Arc::clone(&environment),
        PathUri::from_abs_path(&cwd),
        Some(shell.clone()),
        /*allow_login_shell*/ false,
        codex_protocol::config_types::ShellEnvironmentPolicy::default(),
        /*sandbox*/ None,
    )
    .await
    .ok_or_else(|| anyhow::anyhow!("test shell snapshot must be created"))?;
    let retained_snapshot = Arc::downgrade(&snapshot);
    let request = test_exec_request(
        &turn,
        shell.derive_exec_args("sleep 30", /*use_login_shell*/ false),
        cwd,
        shell_env(),
    );
    let manager = UnifiedExecProcessManager::default();
    let process = manager
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tool_ctx*/ None,
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            /*network_policy_decider*/ None,
            /*tty*/ false,
            Box::new(NoopSpawnLifecycle),
            Some(snapshot),
            environment.as_ref(),
        )
        .await?;
    // Neither abandoning the caller's handle nor an additional shared owner can
    // release the replay file while the manager still owns the live process.
    let shared_process = Arc::clone(&process);
    drop(process);
    assert!(retained_snapshot.upgrade().is_some());
    drop(shared_process);
    assert!(retained_snapshot.upgrade().is_some());
    manager.shutdown_durably().await?;
    assert!(retained_snapshot.upgrade().is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_pipe_commands_preserve_exit_code() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-lc".to_string(), "exit 17".to_string()],
        cwd,
        shell_env(),
    );

    let environment = codex_exec_server::Environment::default_for_tests();
    let process = UnifiedExecProcessManager::default()
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tool_ctx*/ None,
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            /*network_policy_decider*/ None,
            /*tty*/ false,
            Box::new(NoopSpawnLifecycle),
            /*shell_snapshot_file*/ None,
            &environment,
        )
        .await?;

    if !process.has_exited() {
        let exit_signal = process.cancellation_token();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), exit_signal.cancelled())
                .await
                .is_ok(),
            "process did not report exit within timeout"
        );
    }

    assert!(process.has_exited());
    assert_eq!(process.exit_code(), Some(17));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_uses_remote_exec_server_when_configured() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let remote_test_env = remote_test_env().await?;
    let (_, turn) = make_session_and_context().await;
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-i".to_string()],
        remote_test_env.cwd().clone(),
        shell_env(),
    );

    let manager = UnifiedExecProcessManager::default();
    let process = manager
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tool_ctx*/ None,
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            /*network_policy_decider*/ None,
            /*tty*/ true,
            Box::new(NoopSpawnLifecycle),
            /*shell_snapshot_file*/ None,
            remote_test_env.environment(),
        )
        .await?;

    process.write(b"printf 'remote-unified-exec\\n'\n").await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let collected = super::output_collection::collect_output_until_deadline(
        process.output_handles(),
        /*pause_state*/ None,
        Instant::now().checked_add(Duration::from_millis(2_500)),
        None,
    )
    .await
    .collected
    .to_bytes_with_omission_marker();

    assert!(String::from_utf8_lossy(&collected).contains("remote-unified-exec"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_exec_server_rejects_inherited_fd_launches() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let remote_test_env = remote_test_env().await?;
    let (_, mut turn) = make_session_and_context().await;
    let TurnEnvironmentState::Ready(environment) = &mut turn.initial_environments.environments[0]
    else {
        panic!("expected ready primary environment");
    };
    environment.environment = Arc::new(remote_test_env.environment().clone());

    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-lc".to_string(), "echo ok".to_string()],
        cwd,
        shell_env(),
    );

    let manager = UnifiedExecProcessManager::default();
    let err = manager
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tool_ctx*/ None,
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            /*network_policy_decider*/ None,
            /*tty*/ true,
            Box::new(TestSpawnLifecycle {
                inherited_fds: vec![42],
            }),
            /*shell_snapshot_file*/ None,
            turn.initial_environments
                .primary()
                .expect("turn environment")
                .environment
                .as_ref(),
        )
        .await
        .expect_err("expected inherited fd rejection");

    assert_eq!(
        err.to_string(),
        "Failed to create unified exec process: remote exec-server does not support inherited file descriptors"
    );
    Ok(())
}

#[tokio::test]
async fn stdin_approval_preserves_the_reviewed_terminal() -> anyhow::Result<()> {
    use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
    use crate::tools::sandboxing::ToolError;
    use anyhow::Context;
    use codex_features::Feature;
    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ReviewDecision;

    skip_if_sandbox!(Ok(()));
    let (session, turn, events) = make_session_and_context_with_auth_and_config_and_rx(
        codex_login::CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.features.enable(Feature::WriteStdinApproval).unwrap();
            config.permissions.approval_policy =
                crate::config::Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        },
    )
    .await;
    crate::session::approval_test_support::start_approval_turn(&session, &turn, &events).await;
    let manager = &session.services.unified_exec_manager;
    let command = "while IFS= read -r line; do printf 'received:%s\\n' \"$line\"; done";
    let opened = exec_command(
        &session, &turn, command, /*yield_time_ms*/ 250, /*workdir*/ None,
    )
    .await?;
    let process_id = opened.process_id.expect("running terminal");
    let cwd = PathUri::parse("file:///C:/workspace")?;
    let original = {
        let mut store = manager.process_store.lock().await;
        let entry = store.processes.get_mut(&process_id).unwrap();
        entry.permissions = TerminalPermissions::for_launch(
            turn.initial_environments
                .primary()
                .expect("turn environment"),
            &turn,
            TerminalSandboxSource::Native,
            SandboxPermissions::RequireEscalated,
            /*additional_permissions*/ None,
            /*internal_permissions*/ None,
        );
        entry.environment_id = "unselected-executor".to_string();
        entry.cwd = cwd.clone();
        Arc::clone(&entry.process)
    };
    // A queued write must acquire the terminal lock before reading active strict
    // mode: code-mode calls can enable it while another interaction is draining.
    {
        let interaction = original.interaction_lock().lock_owned().await;
        let _active_turn = session.active_turn.lock().await;
        let mut queued = Box::pin(write_stdin(
            &session, &turn, process_id, "queued\n", /*yield_time_ms*/ 250,
        ));
        let mut task_context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(queued.as_mut().poll(&mut task_context).is_pending());
        drop(interaction);
        assert!(queued.as_mut().poll(&mut task_context).is_pending());
        assert!(original.interaction_lock().try_lock_owned().is_err());
    }
    // Empty polling must complete without an approval response. Its requested 250ms
    // yield is clamped to the empty-poll minimum, so the watchdog must start beyond
    // that intentional wait rather than racing the same five-second deadline.
    let empty_poll_wait = Duration::from_millis(
        manager.effective_write_stdin_yield_time_ms("", /*yield_time_ms*/ 250),
    );
    let polled = tokio::time::timeout(
        empty_poll_wait + Duration::from_secs(/*secs*/ 5),
        write_stdin(&session, &turn, process_id, "", /*yield_time_ms*/ 250),
    )
    .await
    .context("empty terminal poll did not complete after its effective yield deadline")??;
    assert_eq!(polled.process_id, Some(process_id));
    let input = "rejected\n";
    let denied = write_stdin(
        &session, &turn, process_id, input, /*yield_time_ms*/ 250,
    )
    .await;
    assert!(
        matches!(denied, Err(UnifiedExecError::StdinApproval(ToolError::Rejected(reason)))
        if reason.contains("select it before retrying"))
    );
    update_current_turn_settings_for_test(&turn, |settings| {
        update_selected_settings_for_test(settings, |selected| {
            selected.approvals_reviewer = ApprovalsReviewer::User;
        });
    });
    assert!(matches!(
        write_stdin(&session, &turn, process_id, input, /*yield_time_ms*/ 250).await,
        Err(UnifiedExecError::StdinApproval(ToolError::Rejected(reason)))
            if reason.contains("select it before retrying")
    ));
    manager
        .process_store
        .lock()
        .await
        .processes
        .get_mut(&process_id)
        .unwrap()
        .environment_id = turn
        .initial_environments
        .primary()
        .unwrap()
        .selection
        .environment_id
        .clone();
    for (input, decision) in [
        ("rejected\n", ReviewDecision::denied("test denial")),
        ("accepted\n", ReviewDecision::Approved),
        ("replace\n", ReviewDecision::Approved),
    ] {
        let review = async {
            loop {
                let EventMsg::ExecApprovalRequest(approval) = events.recv().await?.msg else {
                    continue;
                };
                assert_eq!(
                    (approval.call_id.as_str(), approval.cwd.clone()),
                    ("call", cwd.clone().into())
                );
                assert!(approval.approval_id.is_some());
                assert!(original.interaction_lock().try_lock_owned().is_err());
                if input == "replace\n" {
                    let replacement = super::process_tests::remote_process(
                        WriteStatus::Accepted,
                        /*terminate_error*/ None,
                        SandboxType::None,
                    )
                    .await;
                    let mut store = manager.process_store.lock().await;
                    store.processes.get_mut(&process_id).unwrap().process = Arc::new(replacement);
                }
                crate::session::approval_test_support::respond_to_approval(
                    &session, &approval, decision,
                )
                .await;
                return anyhow::Ok(());
            }
        };
        let (result, reviewed) = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
            tokio::join!(
                write_stdin(
                    &session, &turn, process_id, input, /*yield_time_ms*/ 250
                ),
                review
            )
        })
        .await?;
        reviewed?;
        match input {
            "rejected\n" => assert!(matches!(result, Err(UnifiedExecError::StdinApproval(_)))),
            "accepted\n" => {
                let output = result?.truncated_output(DEFAULT_MAX_OUTPUT_TOKENS);
                assert!(output.contains("received:accepted"), "{output}");
                assert!(!output.contains("received:rejected"), "{output}");
            }
            _ => assert!(
                matches!(result, Err(UnifiedExecError::UnknownProcessId { process_id: id }) if id == process_id)
            ),
        }
        assert!(original.interaction_lock().try_lock_owned().is_ok());
    }
    original.terminate();
    assert!(session.terminate_background_terminal(process_id).await);
    Ok(())
}
