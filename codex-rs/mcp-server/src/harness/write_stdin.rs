use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use crate::harness::process_manager::HarnessProcessManager;
use crate::harness::types::WriteStdinParams;
use crate::harness::types::WriteStdinResponse;

pub async fn handle_write_stdin(
    params: WriteStdinParams,
    process_manager: &HarnessProcessManager,
) -> WriteStdinResponse {
    let session_arc = match process_manager.get_session(params.session_id) {
        Some(s) => s,
        None => {
            let session_id = params.session_id;
            return WriteStdinResponse {
                status: "not_found".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                output: format!("Session {session_id} not found or already terminated"),
                output_truncated: false,
                exit_code: None,
                pid: None,
                session_id: None,
            };
        }
    };

    // 1. Send signal if requested
    if let Some(sig) = params.signal {
        let mut session = session_arc.lock().await;
        if let Err(e) = session.send_signal(sig) {
            let exit_code = session.get_exit_code();
            let pid = session.pid();
            return WriteStdinResponse {
                status: "error".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                output: format!("Failed to send signal {sig:?}: {e}"),
                output_truncated: false,
                exit_code,
                pid,
                session_id: Some(params.session_id),
            };
        }
    }

    // 2. Write characters if provided
    let chars_bytes = params.chars.as_deref().unwrap_or("").as_bytes();
    let close_stdin = params.close_stdin.unwrap_or(false);
    if !chars_bytes.is_empty() || close_stdin {
        let sender = {
            let session = session_arc.lock().await;
            session.process.writer_sender()
        };

        if !chars_bytes.is_empty()
            && let Err(_) = sender.send(chars_bytes.to_vec()).await
        {
            let mut session = session_arc.lock().await;
            let exit_code = session.get_exit_code();
            let pid = session.pid();
            return WriteStdinResponse {
                status: "error".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                output: "Failed to write to stdin: stdin is closed".to_string(),
                output_truncated: false,
                exit_code,
                pid,
                session_id: Some(params.session_id),
            };
        }

        let mut session = session_arc.lock().await;
        if close_stdin {
            session.process.close_stdin();
        }
        session.last_activity_at = Instant::now();
    }

    // 3. Waiting logic: wait_until_exit vs yield_time_ms
    let timeout_ms = params.timeout_ms.unwrap_or(120_000);
    let wait_until_exit = params.wait_until_exit.unwrap_or(false);
    let (initial_stdout_len, initial_stderr_len) = {
        let session = session_arc.lock().await;
        (session.stdout_offset, session.stderr_offset)
    };

    if wait_until_exit {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            let exited = {
                let mut session = session_arc.lock().await;
                session.get_exit_code().is_some()
            };
            if exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    } else {
        let yield_ms = params.yield_time_ms.unwrap_or(2_000);
        let deadline = Instant::now() + Duration::from_millis(yield_ms);
        while Instant::now() < deadline {
            let (exited, has_new_output) = {
                let mut session = session_arc.lock().await;
                let exited = session.get_exit_code().is_some();
                let b = session
                    .buffers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let has_output =
                    b.stdout.len() > initial_stdout_len || b.stderr.len() > initial_stderr_len;
                (exited, has_output)
            };
            if exited || has_new_output {
                tokio::time::sleep(Duration::from_millis(50)).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Allow brief drain of pipe readers
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (exit_code, stdout, stderr, output, output_truncated, pid) = {
        let mut session = session_arc.lock().await;
        let exit_code = session.get_exit_code();
        let (stdout, stderr, output, output_truncated, _total) = session.read_delta();
        let pid = session.pid();
        (exit_code, stdout, stderr, output, output_truncated, pid)
    };

    if let Some(code) = exit_code {
        process_manager.remove_session(params.session_id);
        WriteStdinResponse {
            status: "completed".to_string(),
            stdout,
            stderr,
            output,
            output_truncated,
            exit_code: Some(code),
            pid,
            session_id: None,
        }
    } else {
        WriteStdinResponse {
            status: "running".to_string(),
            stdout,
            stderr,
            output,
            output_truncated,
            exit_code: None,
            pid,
            session_id: Some(params.session_id),
        }
    }
}
