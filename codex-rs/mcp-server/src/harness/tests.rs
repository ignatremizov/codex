use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::models::PermissionProfile;
use codex_utils_json_to_toml::json_to_toml;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::codex_tool_config::CodexToolCallSandboxMode;
use crate::harness::exec_command::resolve_sandbox_ceiling;
use crate::harness::exec_command::sandbox_ceiling_from_profile;
use crate::harness::process_manager::start_output_collectors;
use crate::harness::types::ProcessSignalParam;
use crate::harness::*;

async fn build_test_config(codex_home: std::path::PathBuf, sandbox_mode: &str) -> Config {
    ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(vec![(
            "sandbox_mode".to_string(),
            json_to_toml(serde_json::json!(sandbox_mode)),
        )])
        .build()
        .await
        .expect("build config")
}

#[test]
fn test_tool_schemas_and_properties() {
    let exec_tool = create_tool_for_exec_command();
    assert_eq!(exec_tool.name, "exec_command");
    let exec_json = serde_json::to_value(&exec_tool).expect("exec tool schema serializes");
    let exec_props = exec_json["inputSchema"]["properties"]
        .as_object()
        .expect("exec properties object");
    assert!(exec_props.contains_key("command"));
    assert!(exec_props.contains_key("cwd"));
    assert!(exec_props.contains_key("env"));
    assert!(exec_props.contains_key("shell"));
    assert!(exec_props.contains_key("sandbox"));
    assert!(exec_props.contains_key("yieldTimeMs"));
    assert!(exec_props.contains_key("timeoutMs"));
    assert!(exec_props.contains_key("tty"));
    assert!(exec_props.contains_key("maxOutputBytes"));
    let exec_req = exec_json["inputSchema"]["required"]
        .as_array()
        .expect("exec required array");
    assert!(exec_req.iter().any(|v| v == "command"));

    let stdin_tool = create_tool_for_write_stdin();
    assert_eq!(stdin_tool.name, "write_stdin");
    let stdin_json = serde_json::to_value(&stdin_tool).expect("stdin tool schema serializes");
    let stdin_props = stdin_json["inputSchema"]["properties"]
        .as_object()
        .expect("stdin properties object");
    assert!(stdin_props.contains_key("sessionId"));
    assert!(stdin_props.contains_key("chars"));
    assert!(stdin_props.contains_key("signal"));
    assert!(stdin_props.contains_key("yieldTimeMs"));
    assert!(stdin_props.contains_key("waitUntilExit"));
    assert!(stdin_props.contains_key("timeoutMs"));
    assert!(stdin_props.contains_key("closeStdin"));
    let stdin_req = stdin_json["inputSchema"]["required"]
        .as_array()
        .expect("stdin required array");
    assert!(stdin_req.iter().any(|v| v == "sessionId"));

    let patch_tool = create_tool_for_apply_patch();
    assert_eq!(patch_tool.name, "apply_patch");
    let patch_json = serde_json::to_value(&patch_tool).expect("patch tool schema serializes");
    let patch_props = patch_json["inputSchema"]["properties"]
        .as_object()
        .expect("patch properties object");
    assert!(patch_props.contains_key("patch"));
    assert!(patch_props.contains_key("cwd"));
    assert!(patch_props.contains_key("checkOnly"));
    let patch_req = patch_json["inputSchema"]["required"]
        .as_array()
        .expect("patch required array");
    assert!(patch_req.iter().any(|v| v == "patch"));
}

#[test]
fn test_sandbox_ceiling_resolution_logic() {
    // ReadOnly ceiling
    assert_eq!(
        resolve_sandbox_ceiling(None, SandboxMode::ReadOnly).unwrap(),
        SandboxMode::ReadOnly
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::ReadOnly),
            SandboxMode::ReadOnly
        )
        .unwrap(),
        SandboxMode::ReadOnly
    );
    assert!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::WorkspaceWrite),
            SandboxMode::ReadOnly
        )
        .is_err()
    );
    assert!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::DangerFullAccess),
            SandboxMode::ReadOnly
        )
        .is_err()
    );

    // WorkspaceWrite ceiling
    assert_eq!(
        resolve_sandbox_ceiling(None, SandboxMode::WorkspaceWrite).unwrap(),
        SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::ReadOnly),
            SandboxMode::WorkspaceWrite
        )
        .unwrap(),
        SandboxMode::ReadOnly
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::WorkspaceWrite),
            SandboxMode::WorkspaceWrite
        )
        .unwrap(),
        SandboxMode::WorkspaceWrite
    );
    assert!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::DangerFullAccess),
            SandboxMode::WorkspaceWrite
        )
        .is_err()
    );

    // DangerFullAccess ceiling
    assert_eq!(
        resolve_sandbox_ceiling(None, SandboxMode::DangerFullAccess).unwrap(),
        SandboxMode::DangerFullAccess
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::ReadOnly),
            SandboxMode::DangerFullAccess
        )
        .unwrap(),
        SandboxMode::ReadOnly
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::WorkspaceWrite),
            SandboxMode::DangerFullAccess
        )
        .unwrap(),
        SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        resolve_sandbox_ceiling(
            Some(CodexToolCallSandboxMode::DangerFullAccess),
            SandboxMode::DangerFullAccess
        )
        .unwrap(),
        SandboxMode::DangerFullAccess
    );
}

#[test]
fn test_sandbox_ceiling_from_profile_logic() {
    let read_only = PermissionProfile::read_only();
    assert_eq!(
        sandbox_ceiling_from_profile(&read_only),
        SandboxMode::ReadOnly
    );

    let disabled = PermissionProfile::Disabled;
    assert_eq!(
        sandbox_ceiling_from_profile(&disabled),
        SandboxMode::DangerFullAccess
    );
}

#[tokio::test]
async fn test_harness_process_manager_basics() {
    let manager = HarnessProcessManager::new();
    let id1 = manager.allocate_session_id();
    let id2 = manager.allocate_session_id();
    assert_eq!(id1, 100);
    assert_eq!(id2, 101);

    assert!(manager.get_session(id1).is_none());
    assert!(manager.remove_session(id1).is_none());
}

#[tokio::test]
async fn test_output_buffers_collection_and_truncation() {
    let (stdout_tx, stdout_rx) = mpsc::channel(16);
    let (stderr_tx, stderr_rx) = mpsc::channel(16);

    let max_bytes = 10;
    let buffers = start_output_collectors(stdout_rx, stderr_rx, max_bytes);

    stdout_tx.send(b"hello ".to_vec()).await.unwrap();
    stdout_tx.send(b"world truncated".to_vec()).await.unwrap();
    stderr_tx.send(b"err".to_vec()).await.unwrap();

    drop(stdout_tx);
    drop(stderr_tx);

    tokio::time::sleep(Duration::from_millis(100)).await;

    let b = buffers.lock().unwrap();
    assert_eq!(b.stdout.len(), 10);
    assert_eq!(String::from_utf8_lossy(&b.stdout), "hello worl");
    assert!(b.stdout_truncated);
    assert!(b.total_stdout_bytes > 10);

    assert_eq!(String::from_utf8_lossy(&b.stderr), "err");
    assert!(!b.stderr_truncated);
    assert_eq!(b.total_stderr_bytes, 3);
}

#[tokio::test]
async fn test_exec_command_run_to_completion() {
    let tmp = tempdir().expect("tempdir");
    let config = build_test_config(tmp.path().to_path_buf(), "danger-full-access").await;

    let arg0_paths = Arg0DispatchPaths::default();
    let manager = HarnessProcessManager::new();

    let params = ExecCommandParams {
        command: "echo 'hello from harness'".to_string(),
        cwd: Some(tmp.path().to_string_lossy().to_string()),
        env: None,
        shell: None,
        sandbox: Some(CodexToolCallSandboxMode::DangerFullAccess),
        yield_time_ms: None,
        timeout_ms: Some(10_000),
        tty: None,
        max_output_bytes: None,
    };

    let resp = handle_exec_command(params, &config, &arg0_paths, &manager).await;
    assert_eq!(resp.status, "completed");
    assert_eq!(resp.exit_code, Some(0));
    assert!(resp.output.contains("hello from harness"));
    assert_eq!(resp.session_id, None);
    assert!(!resp.output_truncated);
}

#[tokio::test]
async fn test_exec_command_custom_env_and_exit_code() {
    let tmp = tempdir().expect("tempdir");
    let config = build_test_config(tmp.path().to_path_buf(), "danger-full-access").await;

    let arg0_paths = Arg0DispatchPaths::default();
    let manager = HarnessProcessManager::new();

    let mut custom_env = HashMap::new();
    custom_env.insert(
        "TEST_KEY_HARNESS".to_string(),
        "CUSTOM_VALUE_XYZ".to_string(),
    );

    let params = ExecCommandParams {
        command: "echo val=$TEST_KEY_HARNESS && exit 7".to_string(),
        cwd: Some(tmp.path().to_string_lossy().to_string()),
        env: Some(custom_env),
        shell: None,
        sandbox: Some(CodexToolCallSandboxMode::DangerFullAccess),
        yield_time_ms: None,
        timeout_ms: Some(10_000),
        tty: None,
        max_output_bytes: None,
    };

    let resp = handle_exec_command(params, &config, &arg0_paths, &manager).await;
    assert_eq!(resp.status, "completed");
    assert_eq!(resp.exit_code, Some(7));
    assert!(resp.output.contains("val=CUSTOM_VALUE_XYZ"));
}

#[tokio::test]
async fn test_exec_command_sandbox_ceiling_rejection() {
    let tmp = tempdir().expect("tempdir");
    let config = build_test_config(tmp.path().to_path_buf(), "read-only").await;

    let arg0_paths = Arg0DispatchPaths::default();
    let manager = HarnessProcessManager::new();

    let params = ExecCommandParams {
        command: "echo test".to_string(),
        cwd: None,
        env: None,
        shell: None,
        sandbox: Some(CodexToolCallSandboxMode::DangerFullAccess),
        yield_time_ms: None,
        timeout_ms: None,
        tty: None,
        max_output_bytes: None,
    };

    let resp = handle_exec_command(params, &config, &arg0_paths, &manager).await;
    assert_eq!(resp.status, "rejected");
    assert!(resp.rejection_reason.is_some());
}

#[tokio::test]
async fn test_interactive_session_yield_and_write_stdin_flow() {
    let tmp = tempdir().expect("tempdir");
    let config = build_test_config(tmp.path().to_path_buf(), "danger-full-access").await;

    let arg0_paths = Arg0DispatchPaths::default();
    let manager = HarnessProcessManager::new();

    // 1. Start `cat` which waits on stdin, with yield_time_ms: 150ms
    let params = ExecCommandParams {
        command: "cat".to_string(),
        cwd: Some(tmp.path().to_string_lossy().to_string()),
        env: None,
        shell: None,
        sandbox: Some(CodexToolCallSandboxMode::DangerFullAccess),
        yield_time_ms: Some(150),
        timeout_ms: Some(10_000),
        tty: None,
        max_output_bytes: None,
    };

    let resp = handle_exec_command(params, &config, &arg0_paths, &manager).await;
    assert_eq!(resp.status, "running");
    let session_id = resp.session_id.expect("session_id present");

    // 2. Write line into cat stdin
    let write_params = WriteStdinParams {
        session_id,
        chars: Some("interactive ping\n".to_string()),
        signal: None,
        yield_time_ms: Some(2_000),
        wait_until_exit: Some(false),
        timeout_ms: None,
        close_stdin: Some(false),
    };

    let write_resp = handle_write_stdin(write_params, &manager).await;
    assert_eq!(write_resp.status, "running");
    assert!(write_resp.output.contains("interactive ping"));

    // 3. Close stdin with EOF and wait until exit
    let close_params = WriteStdinParams {
        session_id,
        chars: None,
        signal: None,
        yield_time_ms: None,
        wait_until_exit: Some(true),
        timeout_ms: Some(5_000),
        close_stdin: Some(true),
    };

    let close_resp = handle_write_stdin(close_params, &manager).await;
    assert_eq!(close_resp.status, "completed");
    assert_eq!(close_resp.exit_code, Some(0));
    assert_eq!(close_resp.session_id, None);

    // 4. Subsequent poll on removed session returns not_found
    let poll_params = WriteStdinParams {
        session_id,
        chars: None,
        signal: None,
        yield_time_ms: Some(0),
        wait_until_exit: None,
        timeout_ms: None,
        close_stdin: None,
    };
    let poll_resp = handle_write_stdin(poll_params, &manager).await;
    assert_eq!(poll_resp.status, "not_found");
}

#[tokio::test]
async fn test_write_stdin_signal_termination() {
    let tmp = tempdir().expect("tempdir");
    let config = build_test_config(tmp.path().to_path_buf(), "danger-full-access").await;

    let arg0_paths = Arg0DispatchPaths::default();
    let manager = HarnessProcessManager::new();

    // Start a long-running sleep command
    let params = ExecCommandParams {
        command: "sleep 60".to_string(),
        cwd: Some(tmp.path().to_string_lossy().to_string()),
        env: None,
        shell: None,
        sandbox: Some(CodexToolCallSandboxMode::DangerFullAccess),
        yield_time_ms: Some(150),
        timeout_ms: Some(10_000),
        tty: None,
        max_output_bytes: None,
    };

    let resp = handle_exec_command(params, &config, &arg0_paths, &manager).await;
    assert_eq!(resp.status, "running");
    let session_id = resp.session_id.expect("session_id present");

    // Send SIGKILL to terminate the sleep process
    let sig_params = WriteStdinParams {
        session_id,
        chars: None,
        signal: Some(ProcessSignalParam::Sigkill),
        yield_time_ms: None,
        wait_until_exit: Some(true),
        timeout_ms: Some(5_000),
        close_stdin: None,
    };

    let sig_resp = handle_write_stdin(sig_params, &manager).await;
    assert_eq!(sig_resp.status, "completed");
    assert_eq!(sig_resp.session_id, None);
}

#[tokio::test]
async fn test_apply_patch_dry_run_and_execution() {
    let tmp = tempdir().expect("tempdir");
    let cwd_str = tmp.path().to_string_lossy().to_string();

    let patch = r#"*** Begin Patch
*** Add File: hello.txt
+Hello from patch test!
*** End Patch"#;

    // 1. Dry run validation (check_only: true)
    let check_params = ApplyPatchParams {
        patch: patch.to_string(),
        cwd: Some(cwd_str.clone()),
        check_only: Some(true),
    };

    let check_resp = handle_apply_patch(check_params).await;
    assert!(check_resp.success);
    assert_eq!(check_resp.files.len(), 1);
    assert_eq!(check_resp.files[0].action, "created");
    assert!(!tmp.path().join("hello.txt").exists());

    // 2. Real application (check_only: false)
    let apply_params = ApplyPatchParams {
        patch: patch.to_string(),
        cwd: Some(cwd_str.clone()),
        check_only: Some(false),
    };

    let apply_resp = handle_apply_patch(apply_params).await;
    assert!(apply_resp.success);
    assert_eq!(apply_resp.files.len(), 1);
    assert_eq!(apply_resp.files[0].action, "created");
    assert!(tmp.path().join("hello.txt").exists());
    assert_eq!(
        fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
        "Hello from patch test!\n"
    );

    // 3. Update existing file
    let update_patch = r#"*** Begin Patch
*** Update File: hello.txt
@@
-Hello from patch test!
+Updated patch content!
*** End Patch"#;

    let update_params = ApplyPatchParams {
        patch: update_patch.to_string(),
        cwd: Some(cwd_str),
        check_only: Some(false),
    };

    let update_resp = handle_apply_patch(update_params).await;
    assert!(update_resp.success);
    assert_eq!(update_resp.files.len(), 1);
    assert_eq!(update_resp.files[0].action, "modified");
    assert_eq!(
        fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
        "Updated patch content!\n"
    );
}

#[tokio::test]
async fn test_apply_patch_invalid_patch() {
    let tmp = tempdir().expect("tempdir");
    let cwd_str = tmp.path().to_string_lossy().to_string();

    let invalid_params = ApplyPatchParams {
        patch: "not a valid patch at all".to_string(),
        cwd: Some(cwd_str),
        check_only: Some(true),
    };

    let resp = handle_apply_patch(invalid_params).await;
    assert!(!resp.success);
}
