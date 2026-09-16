use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;
use std::time::Instant;

use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxDirectSpawnTransformRequest;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::WindowsSandboxProxySettingsMode;
use codex_utils_path_uri::PathUri;
use codex_utils_pty::TerminalSize;
use codex_utils_pty::spawn_pipe_process;
use codex_utils_pty::spawn_pty_process;

use crate::codex_tool_config::CodexToolCallSandboxMode;
use crate::harness::process_manager::DEFAULT_MAX_OUTPUT_BYTES;
use crate::harness::process_manager::HarnessProcessManager;
use crate::harness::process_manager::start_output_collectors;
use crate::harness::types::ExecCommandParams;
use crate::harness::types::ExecCommandResponse;

pub(crate) fn resolve_sandbox_ceiling(
    requested: Option<CodexToolCallSandboxMode>,
    server_ceiling: SandboxMode,
) -> Result<SandboxMode, String> {
    let effective: SandboxMode = requested.map(Into::into).unwrap_or(server_ceiling);
    let effective_level = sandbox_rank(&effective);
    let ceiling_level = sandbox_rank(&server_ceiling);

    if effective_level > ceiling_level {
        return Err(format!(
            "Requested sandbox mode '{effective:?}' exceeds server ceiling '{server_ceiling:?}'"
        ));
    }
    Ok(effective)
}

fn sandbox_rank(mode: &SandboxMode) -> u8 {
    match mode {
        SandboxMode::ReadOnly => 1,
        SandboxMode::WorkspaceWrite => 2,
        SandboxMode::DangerFullAccess => 3,
    }
}

pub async fn handle_exec_command(
    params: ExecCommandParams,
    runtime_config: &Config,
    arg0_paths: &Arg0DispatchPaths,
    process_manager: &HarnessProcessManager,
) -> ExecCommandResponse {
    let start_time = Instant::now();

    // 1. Sandbox ceiling check
    let effective_sandbox =
        match resolve_sandbox_ceiling(params.sandbox, runtime_config.sandbox_mode) {
            Ok(mode) => mode,
            Err(rejection_reason) => {
                return ExecCommandResponse {
                    status: "rejected".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: String::new(),
                    output_truncated: false,
                    output_bytes_total: 0,
                    exit_code: None,
                    pid: None,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: Some(rejection_reason),
                };
            }
        };

    // 2. Prepare command and shell tokens
    let shell_str = params.shell.as_deref().unwrap_or("/bin/bash -lc");
    let mut shell_tokens = match shlex::split(shell_str) {
        Some(tokens) if !tokens.is_empty() => tokens,
        _ => vec!["/bin/bash".to_string(), "-lc".to_string()],
    };
    let program = shell_tokens.remove(0);
    let mut args = shell_tokens;
    args.push(params.command.clone());

    // 3. Starlark execution policy evaluation
    if let Ok(policy) = codex_core::load_exec_policy(&runtime_config.config_layer_stack).await {
        let eval_cmd = vec![params.command.clone()];
        let eval = policy.check(&eval_cmd, &|_| codex_execpolicy::Decision::Allow);
        match eval.decision {
            codex_execpolicy::Decision::Forbidden => {
                return ExecCommandResponse {
                    status: "rejected".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: String::new(),
                    output_truncated: false,
                    output_bytes_total: 0,
                    exit_code: None,
                    pid: None,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: Some(
                        "Command was rejected by Starlark execution policy".to_string(),
                    ),
                };
            }
            codex_execpolicy::Decision::Prompt => {
                return ExecCommandResponse {
                    status: "rejected".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: String::new(),
                    output_truncated: false,
                    output_bytes_total: 0,
                    exit_code: None,
                    pid: None,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: Some(
                        "Command requires user approval under Starlark execution policy. Please add an approval rule in ~/.codex/rules/".to_string(),
                    ),
                };
            }
            codex_execpolicy::Decision::Allow => {}
        }
    }

    // 4. Dangerous command platform check
    let command_tokens =
        shlex::split(&params.command).unwrap_or_else(|| vec![params.command.clone()]);
    if codex_shell_command::is_dangerous_command::dangerous_command_match(&command_tokens).is_some()
    {
        return ExecCommandResponse {
            status: "rejected".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            output: String::new(),
            output_truncated: false,
            output_bytes_total: 0,
            exit_code: None,
            pid: None,
            session_id: None,
            wall_time_ms: start_time.elapsed().as_millis() as u64,
            rejection_reason: Some(
                "Command matched dangerous destructive command protection rules".to_string(),
            ),
        };
    }

    // 5. Resolve CWD
    let native_cwd = match &params.cwd {
        Some(dir) => match codex_utils_absolute_path::AbsolutePathBuf::relative_to_current_dir(dir)
        {
            Ok(p) => p,
            Err(e) => {
                return ExecCommandResponse {
                    status: "error".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: format!("Invalid working directory '{dir}': {e}"),
                    output_truncated: false,
                    output_bytes_total: 0,
                    exit_code: None,
                    pid: None,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: None,
                };
            }
        },
        None => match codex_utils_absolute_path::AbsolutePathBuf::current_dir() {
            Ok(p) => p,
            Err(e) => {
                return ExecCommandResponse {
                    status: "error".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: format!("Failed to determine current directory: {e}"),
                    output_truncated: false,
                    output_bytes_total: 0,
                    exit_code: None,
                    pid: None,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: None,
                };
            }
        },
    };
    let cwd_uri = PathUri::from_abs_path(&native_cwd);

    // 6. Inherit and inject environment
    let mut effective_env: HashMap<String, String> = std::env::vars().collect();
    if let Some(extra_env) = params.env {
        for (k, v) in extra_env {
            effective_env.insert(k, v);
        }
    }

    // 7. Sandbox transformation
    let (program_os, args_vec, env_map) = match effective_sandbox {
        SandboxMode::DangerFullAccess => (OsString::from(program), args, effective_env),
        SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly => {
            let permission_profile = match effective_sandbox {
                SandboxMode::ReadOnly => PermissionProfile::read_only(),
                _ => PermissionProfile::workspace_write(),
            };
            let sandbox_manager = SandboxManager::new();
            let sandbox_type = sandbox_manager.select_initial(
                &permission_profile,
                SandboxablePreference::Auto,
                WindowsSandboxLevel::Disabled,
                /*has_managed_network_requirements*/ false,
            );

            if sandbox_type == SandboxType::None {
                (OsString::from(program), args, effective_env)
            } else {
                let direct_request = SandboxDirectSpawnTransformRequest {
                    workspace_roots: std::slice::from_ref(&native_cwd),
                    windows_sandbox_proxy_settings_mode: WindowsSandboxProxySettingsMode::default(),
                    transform: SandboxTransformRequest {
                        command: SandboxCommand {
                            program: OsString::from(program),
                            args,
                            cwd: cwd_uri.clone(),
                            env: effective_env,
                            managed_network: None,
                            additional_permissions: None,
                        },
                        permissions: &permission_profile,
                        sandbox: sandbox_type,
                        enforce_managed_network: false,
                        environment_id: None,
                        network: None,
                        sandbox_policy_cwd: &cwd_uri,
                        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe.as_deref(),
                        use_legacy_landlock: false,
                        windows_sandbox_level: WindowsSandboxLevel::Disabled,
                        windows_sandbox_private_desktop: false,
                    },
                };
                match sandbox_manager.transform_for_direct_spawn(direct_request) {
                    Ok(mut cmd) => {
                        let prog = if !cmd.command.is_empty() {
                            cmd.command.remove(0)
                        } else {
                            program
                        };
                        (OsString::from(prog), cmd.command, cmd.env)
                    }
                    Err(e) => {
                        return ExecCommandResponse {
                            status: "error".to_string(),
                            stdout: String::new(),
                            stderr: String::new(),
                            output: format!("Failed to apply sandbox: {e}"),
                            output_truncated: false,
                            output_bytes_total: 0,
                            exit_code: None,
                            pid: None,
                            session_id: None,
                            wall_time_ms: start_time.elapsed().as_millis() as u64,
                            rejection_reason: None,
                        };
                    }
                }
            }
        }
    };

    // 8. Spawn process
    let program_str = program_os.to_string_lossy();
    let spawn_result = if params.tty.unwrap_or(false) {
        spawn_pty_process(
            program_str.as_ref(),
            &args_vec,
            native_cwd.as_path(),
            &env_map,
            &None,
            TerminalSize { rows: 24, cols: 80 },
            &[],
        )
        .await
    } else {
        spawn_pipe_process(
            &program_os,
            &args_vec,
            native_cwd.as_path(),
            &env_map,
            &None,
            &[],
        )
        .await
    };

    let mut spawned = match spawn_result {
        Ok(s) => s,
        Err(e) => {
            return ExecCommandResponse {
                status: "error".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                output: format!("Failed to spawn process: {e}"),
                output_truncated: false,
                output_bytes_total: 0,
                exit_code: None,
                pid: None,
                session_id: None,
                wall_time_ms: start_time.elapsed().as_millis() as u64,
                rejection_reason: None,
            };
        }
    };

    let pid = spawned.session.pid();
    let max_output_bytes = params.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
    let buffers = start_output_collectors(spawned.stdout_rx, spawned.stderr_rx, max_output_bytes);

    // 9. Execution flow: Wait to completion vs. Yield mode
    match params.yield_time_ms {
        None => {
            // Wait to completion up to timeout_ms
            let timeout_ms = params.timeout_ms.unwrap_or(120_000);
            let timeout_duration = Duration::from_millis(timeout_ms);

            let (status, exit_code) = tokio::select! {
                exit_res = &mut spawned.exit_rx => {
                    ("completed".to_string(), Some(exit_res.unwrap_or(-1)))
                }
                _ = tokio::time::sleep(timeout_duration) => {
                    spawned.session.terminate();
                    ("timed_out".to_string(), Some(124))
                }
            };

            // Allow brief drain of pipe readers
            tokio::time::sleep(Duration::from_millis(50)).await;

            let b = buffers.lock().await;
            let stdout = String::from_utf8_lossy(&b.stdout).to_string();
            let stderr = String::from_utf8_lossy(&b.stderr).to_string();
            let mut output = String::with_capacity(stdout.len() + stderr.len());
            output.push_str(&stdout);
            if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&stderr);

            let truncated = b.stdout_truncated || b.stderr_truncated;
            let total = b.total_stdout_bytes + b.total_stderr_bytes;

            ExecCommandResponse {
                status,
                stdout,
                stderr,
                output,
                output_truncated: truncated,
                output_bytes_total: total,
                exit_code,
                pid,
                session_id: None,
                wall_time_ms: start_time.elapsed().as_millis() as u64,
                rejection_reason: None,
            }
        }
        Some(yield_ms) => {
            let yield_duration = Duration::from_millis(yield_ms);
            let exit_opt = tokio::select! {
                exit_res = &mut spawned.exit_rx => {
                    Some(exit_res.unwrap_or(-1))
                }
                _ = tokio::time::sleep(yield_duration) => {
                    None
                }
            };

            if let Some(code) = exit_opt {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let b = buffers.lock().await;
                let stdout = String::from_utf8_lossy(&b.stdout).to_string();
                let stderr = String::from_utf8_lossy(&b.stderr).to_string();
                let mut output = String::with_capacity(stdout.len() + stderr.len());
                output.push_str(&stdout);
                if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&stderr);

                let truncated = b.stdout_truncated || b.stderr_truncated;
                let total = b.total_stdout_bytes + b.total_stderr_bytes;

                ExecCommandResponse {
                    status: "completed".to_string(),
                    stdout,
                    stderr,
                    output,
                    output_truncated: truncated,
                    output_bytes_total: total,
                    exit_code: Some(code),
                    pid,
                    session_id: None,
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: None,
                }
            } else {
                // Process is still alive; yield session_id
                let session_id = process_manager.allocate_session_id();
                let b = buffers.lock().await;
                let stdout = String::from_utf8_lossy(&b.stdout).to_string();
                let stderr = String::from_utf8_lossy(&b.stderr).to_string();
                let mut output = String::with_capacity(stdout.len() + stderr.len());
                output.push_str(&stdout);
                if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&stderr);

                let stdout_len = b.stdout.len();
                let stderr_len = b.stderr.len();
                let truncated = b.stdout_truncated || b.stderr_truncated;
                let total = b.total_stdout_bytes + b.total_stderr_bytes;
                drop(b);

                process_manager
                    .register_session(
                        session_id,
                        spawned.session,
                        buffers,
                        stdout_len,
                        stderr_len,
                        Some(spawned.exit_rx),
                    )
                    .await;

                ExecCommandResponse {
                    status: "running".to_string(),
                    stdout,
                    stderr,
                    output,
                    output_truncated: truncated,
                    output_bytes_total: total,
                    exit_code: None,
                    pid,
                    session_id: Some(session_id),
                    wall_time_ms: start_time.elapsed().as_millis() as u64,
                    rejection_reason: None,
                }
            }
        }
    }
}
