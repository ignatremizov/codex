use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::ThreadIdleCause;
use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::exec::StdoutStream;
use crate::exec::execute_exec_request;
use crate::exec_env::create_env;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_session_id_env;
use crate::sandboxing::ExecRequest;
use crate::session::turn_context::TurnContext;
use crate::shell::Shell;
use crate::tools::format_exec_output_str;
use crate::tools::runtimes::RuntimePathPrepends;
#[cfg(unix)]
use crate::tools::runtimes::apply_package_path_prepend;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use crate::tools::runtimes::strip_managed_proxy_env;
use crate::unified_exec::UserShellCommandRetirement;
use crate::user_shell_command::user_shell_command_record_item;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::protocol::UserShellCommandResponseHandling;
use codex_sandboxing::SandboxType;
use codex_shell_command::parse_command::parse_command;
use codex_thread_store::PersistContext;

use crate::session::session::Session;
use codex_protocol::models::PermissionProfile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserShellCommandPlacement {
    /// Uses its submission id as a completed activity-history identity without owning a turn.
    Detached,
    /// Reuses an already active turn's presentation identity without inheriting its cancellation.
    ActiveTurn,
}

struct UserShellCommandRegistration {
    session: Arc<Session>,
    process_id: Option<i32>,
    call_id: Option<String>,
    submission_id: Option<u64>,
}

impl UserShellCommandRegistration {
    fn new(session: Arc<Session>, submission_id: u64) -> Self {
        Self {
            session,
            process_id: None,
            call_id: None,
            submission_id: Some(submission_id),
        }
    }

    async fn unregister(&mut self) {
        if let (Some(process_id), Some(call_id)) = (self.process_id, self.call_id.clone()) {
            self.session
                .services
                .unified_exec_manager
                .unregister_user_shell_command(process_id, &call_id)
                .await;
            self.process_id = None;
            self.call_id = None;
        }
        self.release_submission().await;
    }

    async fn retire_process_for_delivery(&mut self) -> UserShellCommandRetirement {
        let (Some(process_id), Some(call_id)) = (self.process_id, self.call_id.as_deref()) else {
            error!("user shell process registration disappeared before result delivery");
            return UserShellCommandRetirement::Stopped;
        };
        let retirement = self
            .session
            .services
            .unified_exec_manager
            .retire_user_shell_command(process_id, call_id)
            .await;
        if retirement.is_some() {
            self.process_id = None;
            self.call_id = None;
        }
        retirement.unwrap_or_else(|| {
            error!("user shell process registry rejected exact result-delivery retirement");
            UserShellCommandRetirement::Stopped
        })
    }

    async fn release_submission(&mut self) {
        let Some(submission_id) = self.submission_id else {
            return;
        };
        self.session
            .services
            .unified_exec_manager
            .release_user_shell_submission(submission_id)
            .await;
        self.submission_id = None;
    }
}

impl Drop for UserShellCommandRegistration {
    fn drop(&mut self) {
        let process = self.process_id.take().zip(self.call_id.take());
        let submission_id = self.submission_id.take();
        if process.is_none() && submission_id.is_none() {
            return;
        }
        let session = Arc::clone(&self.session);
        self.session.services.runtime_handle.spawn(async move {
            if let Some((process_id, call_id)) = process {
                session
                    .services
                    .unified_exec_manager
                    .unregister_user_shell_command(process_id, &call_id)
                    .await;
            }
            if let Some(submission_id) = submission_id {
                session
                    .services
                    .unified_exec_manager
                    .release_user_shell_submission(submission_id)
                    .await;
            }
        });
    }
}

pub(crate) async fn execute_user_shell_command(
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    command: String,
    timeout_ms: Option<u64>,
    placement: UserShellCommandPlacement,
    response_handling: UserShellCommandResponseHandling,
    submission_id: u64,
) {
    let mut registration = UserShellCommandRegistration::new(Arc::clone(&session), submission_id);
    session
        .services
        .session_telemetry
        .counter("codex.task.user_shell", /*inc*/ 1, &[]);

    let Some((turn_environment, environment_shell)) = turn_context
        .environments
        .local()
        .and_then(|environment| environment.shell.as_ref().map(|shell| (environment, shell)))
    else {
        send_user_shell_error(
            &session,
            turn_context.as_ref(),
            "shell is unavailable in this session",
        )
        .await;
        registration.unregister().await;
        if response_handling.final_delivery == UserShellCommandFinalDelivery::Wake {
            session
                .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Failed)
                .await;
        }
        return;
    };

    // Execute the user's script under the environment's shell; this
    // allows commands that use shell features (pipes, &&, redirects, etc.).
    // We do not source rc files or otherwise reformat the script.
    let use_login_shell = true;
    let display_command = environment_shell.derive_exec_args(&command, use_login_shell);
    // TODO(anp): Migrate user-shell events and execution plumbing to PathUri so this local-only
    // feature does not need to project the selected environment cwd onto the Codex host.
    let Ok(cwd) = turn_environment.cwd().to_abs_path() else {
        send_user_shell_error(
            &session,
            turn_context.as_ref(),
            "shell working directory is not native to the Codex host",
        )
        .await;
        registration.unregister().await;
        if response_handling.final_delivery == UserShellCommandFinalDelivery::Wake {
            session
                .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Failed)
                .await;
        }
        return;
    };
    let shell_snapshot_location = turn_environment.shell_snapshot(&cwd);
    let shell_environment_policy = turn_environment.shell_environment_policy();
    let mut exec_env_map = create_env(shell_environment_policy, Some(session.thread_id));
    inject_session_id_env(&mut exec_env_map, session.session_id());
    inject_apply_patch_env(&mut exec_env_map, &turn_context.config.features);
    if exec_env_map.contains_key(PROXY_ACTIVE_ENV_KEY) {
        strip_managed_proxy_env(&mut exec_env_map);
    }
    let exec_command = prepare_user_shell_exec_command(
        &display_command,
        environment_shell,
        shell_snapshot_location.as_ref(),
        &shell_environment_policy.r#set,
        &mut exec_env_map,
    );

    let call_id = match placement {
        UserShellCommandPlacement::Detached => turn_context.sub_id.clone(),
        UserShellCommandPlacement::ActiveTurn => Uuid::new_v4().to_string(),
    };
    let raw_command = command;
    let command_cancellation = CancellationToken::new();
    let process_id = session
        .services
        .unified_exec_manager
        .register_user_shell_command(
            call_id.clone(),
            submission_id,
            raw_command.clone(),
            cwd.clone().into(),
            response_handling,
            command_cancellation.clone(),
        )
        .await;
    registration.process_id = Some(process_id);
    registration.call_id = Some(call_id.clone());
    let process_id_string = process_id.to_string();

    let parsed_cmd = parse_command(&display_command);
    session
        .emit_turn_item_started(
            turn_context.as_ref(),
            &TurnItem::CommandExecution(CommandExecutionItem {
                id: call_id.clone(),
                plugin_id: None,
                script_path: None,
                process_id: Some(process_id_string.clone()),
                command: display_command.clone(),
                cwd: cwd.clone().into(),
                parsed_cmd: parsed_cmd.clone(),
                source: ExecCommandSource::UserShell,
                user_shell_response_handling: Some(response_handling),
                interaction_input: None,
                status: CommandExecutionStatus::InProgress,
                stdout: None,
                stderr: None,
                aggregated_output: None,
                exit_code: None,
                duration: None,
                formatted_output: None,
            }),
        )
        .await;

    let permission_profile = PermissionProfile::Disabled;
    let expiration = match timeout_ms {
        Some(timeout_ms) => {
            ExecExpiration::from(timeout_ms).with_cancellation(command_cancellation.clone())
        }
        None => match turn_context.config.user_shell_command_timeout_ms() {
            0 => ExecExpiration::Cancellation(command_cancellation.clone()),
            timeout_ms => {
                ExecExpiration::from(timeout_ms).with_cancellation(command_cancellation.clone())
            }
        },
    };
    let exec_env = ExecRequest {
        command: exec_command.clone(),
        cwd: cwd.clone().into(),
        env: exec_env_map,
        exec_server_env_config: None,
        exec_server_shell_snapshot: None,
        // `/shell` is the explicit full-access escape hatch, so it must not
        // inherit a managed proxy from the surrounding session or turn.
        network: None,
        network_environment_id: None,
        expiration,
        capture_policy: ExecCapturePolicy::ShellTool,
        sandbox: SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: turn_context.effective_workspace_roots(),
        windows_sandbox_level: turn_context.windows_sandbox_level,
        windows_sandbox_private_desktop: turn_context
            .config
            .permissions
            .windows_sandbox_private_desktop,
        permission_profile,
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: false,
        exec_server_managed_network: None,
        exec_server_network_proxy: None,
    };

    let stdout_stream = Some(StdoutStream {
        sub_id: turn_context.sub_id.clone(),
        call_id: call_id.clone(),
        tx_event: session.get_tx_event(),
    });

    let launch_claimed = session
        .services
        .unified_exec_manager
        .wait_for_user_shell_launch(
            submission_id,
            response_handling.queue_command,
            &command_cancellation,
        )
        .await;
    let launch_allowed = launch_claimed && !command_cancellation.is_cancelled();
    let exec_result = if launch_allowed {
        execute_exec_request(exec_env, stdout_stream, /*after_spawn*/ None).await
    } else {
        let message = "command stopped before it started".to_string();
        Ok(ExecToolCallOutput {
            exit_code: 130,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(message.clone()),
            aggregated_output: StreamOutput::new(message),
            duration: Duration::ZERO,
            timed_out: false,
        })
    };

    let output = match exec_result {
        Ok(output) => output,
        Err(err)
            if matches!(
                err.details(),
                CodexErrorDetails::Sandbox(SandboxErr::Timeout { .. })
            ) =>
        {
            let CodexErrorDetails::Sandbox(SandboxErr::Timeout { output }) = err.details() else {
                unreachable!("guard ensures timeout details");
            };
            output.as_ref().clone()
        }
        Err(err) => {
            error!("user shell command failed: {err:?}");
            let message = format!("execution error: {err:?}");
            ExecToolCallOutput {
                exit_code: -1,
                stdout: StreamOutput::new(String::new()),
                stderr: StreamOutput::new(message.clone()),
                aggregated_output: StreamOutput::new(message),
                duration: Duration::ZERO,
                timed_out: false,
            }
        }
    };

    let retirement = registration.retire_process_for_delivery().await;
    let command_was_stopped = !launch_allowed || retirement == UserShellCommandRetirement::Stopped;
    let start_wake = deliver_user_shell_output(
        &session,
        turn_context.as_ref(),
        &raw_command,
        &output,
        response_handling,
        command_was_stopped,
    )
    .await;
    session
        .emit_turn_item_completed(
            turn_context.as_ref(),
            TurnItem::CommandExecution(CommandExecutionItem {
                id: call_id,
                process_id: Some(process_id_string),
                command: display_command,
                cwd: cwd.into(),
                parsed_cmd,
                source: ExecCommandSource::UserShell,
                user_shell_response_handling: Some(response_handling),
                plugin_id: None,
                script_path: None,
                interaction_input: None,
                status: if output.exit_code == 0 {
                    CommandExecutionStatus::Completed
                } else {
                    CommandExecutionStatus::Failed
                },
                stdout: Some(output.stdout.text.clone()),
                stderr: Some(output.stderr.text.clone()),
                aggregated_output: Some(output.aggregated_output.text.clone()),
                exit_code: Some(output.exit_code),
                duration: Some(output.duration),
                formatted_output: Some(format_exec_output_str(
                    &output,
                    turn_context.model_info().truncation_policy.into(),
                )),
            }),
        )
        .await;
    // A user shell command can finish before the first ordinary model turn, so materialize its
    // completed activity before scheduling an idle wake.
    session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    registration.release_submission().await;
    if start_wake {
        session.maybe_start_turn_for_pending_work().await;
    } else if response_handling.final_delivery == UserShellCommandFinalDelivery::Wake {
        session
            .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
            .await;
    }

    registration.unregister().await;
}

async fn send_user_shell_error(session: &Session, turn_context: &TurnContext, message: &str) {
    session
        .send_event(
            turn_context,
            EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: None,
            }),
        )
        .await;
}

fn prepare_user_shell_exec_command(
    display_command: &[String],
    shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    shell_environment_set: &HashMap<String, String>,
    exec_env_map: &mut HashMap<String, String>,
) -> Vec<String> {
    #[cfg(unix)]
    {
        prepare_user_shell_exec_command_with_path_prepend(
            display_command,
            shell,
            shell_snapshot,
            shell_environment_set,
            exec_env_map,
            apply_package_path_prepend,
        )
    }

    #[cfg(not(unix))]
    {
        maybe_wrap_shell_lc_with_snapshot(
            display_command,
            shell,
            shell_snapshot,
            shell_environment_set,
            exec_env_map,
            // On non-Unix targets, arg0 has already prepended the package path
            // to the process PATH before create_env() builds exec_env_map.
            // RuntimePathPrepends is only needed for Unix shell snapshot replay.
            &RuntimePathPrepends::default(),
        )
    }
}

/// Prepares a user-shell command after adding runtime-owned PATH entries.
///
/// The callback mutates the live exec environment for commands that are not
/// wrapped with a shell snapshot and records only the runtime-owned entries so
/// snapshot wrapping can reapply them after restoring the user's snapshot PATH.
#[cfg(unix)]
fn prepare_user_shell_exec_command_with_path_prepend(
    display_command: &[String],
    shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    shell_environment_set: &HashMap<String, String>,
    exec_env_map: &mut HashMap<String, String>,
    prepend_runtime_path: impl FnOnce(&mut HashMap<String, String>, &mut RuntimePathPrepends),
) -> Vec<String> {
    let explicit_env_overrides = shell_environment_set.clone();
    let mut runtime_path_prepends = RuntimePathPrepends::default();
    prepend_runtime_path(exec_env_map, &mut runtime_path_prepends);
    maybe_wrap_shell_lc_with_snapshot(
        display_command,
        shell,
        shell_snapshot,
        &explicit_env_overrides,
        exec_env_map,
        &runtime_path_prepends,
    )
}

async fn deliver_user_shell_output(
    session: &Arc<Session>,
    turn_context: &TurnContext,
    raw_command: &str,
    exec_output: &ExecToolCallOutput,
    response_handling: UserShellCommandResponseHandling,
    stopped: bool,
) -> bool {
    let output_item = user_shell_command_record_item(raw_command, exec_output, turn_context);
    let final_delivery =
        if stopped && response_handling.final_delivery == UserShellCommandFinalDelivery::Wake {
            UserShellCommandFinalDelivery::Passive
        } else {
            response_handling.final_delivery
        };
    match final_delivery {
        UserShellCommandFinalDelivery::Passive => {
            session
                .inject_no_new_turn(vec![output_item], Some(turn_context))
                .await;
            false
        }
        UserShellCommandFinalDelivery::Wake => {
            session
                .inject_or_queue_response_item_for_wake(output_item, turn_context)
                .await
        }
        UserShellCommandFinalDelivery::PresentationOnly => false,
    }
}

#[cfg(all(test, unix))]
#[path = "user_shell_tests.rs"]
mod tests;
