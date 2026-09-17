use std::sync::Arc;

use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::RequestId;

use crate::harness::apply_patch::handle_apply_patch;
use crate::harness::exec_command::handle_exec_command;
use crate::harness::process_manager::HarnessProcessManager;
use crate::harness::types::ApplyPatchParams;
use crate::harness::types::ExecCommandParams;
use crate::harness::types::WriteStdinParams;
use crate::harness::write_stdin::handle_write_stdin;
use crate::outgoing_message::OutgoingMessageSender;

pub(crate) fn structured_process_response<T: serde::Serialize>(
    response: &T,
) -> Option<serde_json::Value> {
    let mut structured = serde_json::to_value(response).ok()?;
    if let Some(object) = structured.as_object_mut() {
        let stdout = object.get("stdout").and_then(serde_json::Value::as_str);
        let stderr = object.get("stderr").and_then(serde_json::Value::as_str);
        let output = object.get("output").and_then(serde_json::Value::as_str);
        let redundant_output = match (stdout, stderr, output) {
            (Some(stdout), Some(""), Some(output)) => output == stdout,
            (Some(""), Some(stderr), Some(output)) => output == stderr,
            _ => false,
        };
        if redundant_output {
            object.remove("output");
        }
    }
    Some(structured)
}

pub(crate) fn dispatch_harness_tool_call(
    tool_name: &str,
    id: RequestId,
    arguments: Option<rmcp::model::JsonObject>,
    outgoing: Arc<OutgoingMessageSender>,
    runtime_config: Arc<Config>,
    arg0_paths: Arg0DispatchPaths,
    harness_manager: Arc<HarnessProcessManager>,
) {
    let arguments = arguments.map(serde_json::Value::Object);
    match tool_name {
        "exec_command" => {
            tokio::spawn(async move {
                let params: Result<ExecCommandParams, _> = match arguments {
                    Some(val) => serde_json::from_value(val),
                    None => {
                        let result = CallToolResult::error(vec![ContentBlock::text(
                            "Missing arguments for exec_command: 'command' is required.",
                        )]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };
                let params = match params {
                    Ok(p) => p,
                    Err(e) => {
                        let result = CallToolResult::error(vec![ContentBlock::text(format!(
                            "Failed to parse arguments for exec_command: {e}"
                        ))]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };

                let resp =
                    handle_exec_command(params, &runtime_config, &arg0_paths, &harness_manager)
                        .await;
                let is_error = if resp.status == "error" || resp.status == "rejected" {
                    Some(true)
                } else {
                    None
                };
                let text = if !resp.output.is_empty() {
                    resp.output.clone()
                } else if resp.status == "running" {
                    let session_id = resp.session_id.unwrap_or_default();
                    format!("Process running in background with session ID: {session_id}")
                } else if resp.status == "rejected" {
                    resp.rejection_reason
                        .clone()
                        .unwrap_or_else(|| "Command execution rejected".to_string())
                } else {
                    let code = resp.exit_code.unwrap_or(0);
                    format!("Process completed with exit code: {code}")
                };
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.is_error = is_error;
                result.structured_content = structured_process_response(&resp);
                outgoing.send_response(id, result);
            });
        }
        "write_stdin" => {
            tokio::spawn(async move {
                let params: Result<WriteStdinParams, _> = match arguments {
                    Some(val) => serde_json::from_value(val),
                    None => {
                        let result = CallToolResult::error(vec![ContentBlock::text(
                            "Missing arguments for write_stdin: 'sessionId' is required.",
                        )]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };
                let params = match params {
                    Ok(p) => p,
                    Err(e) => {
                        let result = CallToolResult::error(vec![ContentBlock::text(format!(
                            "Failed to parse arguments for write_stdin: {e}"
                        ))]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };

                let resp = handle_write_stdin(params, &harness_manager).await;
                let is_error = if resp.status == "error" || resp.status == "not_found" {
                    Some(true)
                } else {
                    None
                };
                let text = if !resp.output.is_empty() {
                    resp.output.clone()
                } else if resp.status == "completed" {
                    let code = resp.exit_code.unwrap_or(0);
                    format!("Process completed with exit code: {code}")
                } else {
                    let sid = resp.session_id.unwrap_or_default();
                    let st = &resp.status;
                    format!("Session {sid} is {st}")
                };
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.is_error = is_error;
                result.structured_content = structured_process_response(&resp);
                outgoing.send_response(id, result);
            });
        }
        "apply_patch" => {
            tokio::spawn(async move {
                let params: Result<ApplyPatchParams, _> = match arguments {
                    Some(val) => serde_json::from_value(val),
                    None => {
                        let result = CallToolResult::error(vec![ContentBlock::text(
                            "Missing arguments for apply_patch: 'patch' is required.",
                        )]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };
                let params = match params {
                    Ok(p) => p,
                    Err(e) => {
                        let result = CallToolResult::error(vec![ContentBlock::text(format!(
                            "Failed to parse arguments for apply_patch: {e}"
                        ))]);
                        outgoing.send_response(id, result);
                        return;
                    }
                };

                let resp = handle_apply_patch(params).await;
                let is_error = if !resp.success { Some(true) } else { None };
                let text = resp.summary.clone();
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.is_error = is_error;
                result.structured_content = serde_json::to_value(&resp).ok();
                outgoing.send_response(id, result);
            });
        }
        _ => {
            let result = CallToolResult::error(vec![ContentBlock::text(format!(
                "Unknown tool '{tool_name}'"
            ))]);
            outgoing.send_response(id, result);
        }
    }
}
