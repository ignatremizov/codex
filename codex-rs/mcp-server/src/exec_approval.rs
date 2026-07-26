use std::path::PathBuf;
use std::sync::Arc;

use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use rmcp::model::ErrorData;
use rmcp::model::RequestId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tracing::error;

/// Conforms to the MCP elicitation request params shape, so it can be used as
/// the `params` field of an `elicitation/create` request.
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecApprovalElicitRequestParams {
    // These fields are required so that `params`
    // conforms to ElicitRequestParams.
    pub message: String,

    #[serde(rename = "requestedSchema")]
    pub requested_schema: Value,

    // These are additional fields the client can use to
    // correlate the request with the codex tool call.
    #[serde(rename = "threadId")]
    pub thread_id: ThreadId,
    pub codex_elicitation: String,
    pub codex_mcp_tool_call_id: String,
    pub codex_event_id: String,
    pub codex_call_id: String,
    pub codex_command: Vec<String>,
    pub codex_cwd: PathBuf,
    pub codex_parsed_cmd: Vec<ParsedCommand>,
}

// TODO(mbolin): ExecApprovalResponse does not conform to ElicitResult. See:
// - https://github.com/modelcontextprotocol/modelcontextprotocol/blob/f962dc1780fa5eed7fb7c8a0232f1fc83ef220cd/schema/2025-06-18/schema.json#L617-L636
// - https://modelcontextprotocol.io/specification/draft/client/elicitation#protocol-messages
// It should have "action" and "content" fields.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecApprovalResponse {
    pub decision: ReviewDecision,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_exec_approval_request(
    command: Vec<String>,
    cwd: PathBuf,
    outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    codex: Arc<CodexThread>,
    request_id: RequestId,
    tool_call_id: String,
    event_id: String,
    approval_deadline: Option<tokio::time::Instant>,
    call_id: String,
    approval_id: String,
    codex_parsed_cmd: Vec<ParsedCommand>,
    thread_id: ThreadId,
) {
    let escaped_command =
        shlex::try_join(command.iter().map(String::as_str)).unwrap_or_else(|_| command.join(" "));
    let message = format!(
        "Allow Codex to run `{escaped_command}` in `{cwd}`?",
        cwd = cwd.to_string_lossy()
    );

    let params = ExecApprovalElicitRequestParams {
        message,
        requested_schema: json!({"type":"object","properties":{}}),
        thread_id,
        codex_elicitation: "exec-approval".to_string(),
        codex_mcp_tool_call_id: tool_call_id.clone(),
        codex_event_id: event_id.clone(),
        codex_call_id: call_id,
        codex_command: command,
        codex_cwd: cwd,
        codex_parsed_cmd,
    };
    let params_json = match serde_json::to_value(&params) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("Failed to serialize ExecApprovalElicitRequestParams: {err}");
            error!("{message}");

            outgoing.send_error(request_id.clone(), ErrorData::invalid_params(message, None));

            return;
        }
    };

    let (elicitation_request_id, on_response) = outgoing
        .send_request("elicitation/create", Some(params_json))
        .await;

    // Listen for the response on a separate task so we don't block the main agent loop.
    {
        let codex = codex.clone();
        let approval_id = approval_id.clone();
        let event_id = event_id.clone();
        let outgoing = Arc::clone(&outgoing);
        tokio::spawn(async move {
            on_exec_approval_response(
                approval_id,
                event_id,
                elicitation_request_id,
                approval_deadline,
                on_response,
                outgoing,
                codex,
            )
            .await;
        });
    }
}

async fn on_exec_approval_response(
    approval_id: String,
    event_id: String,
    elicitation_request_id: RequestId,
    approval_deadline: Option<tokio::time::Instant>,
    receiver: tokio::sync::oneshot::Receiver<crate::outgoing_message::ClientResponse>,
    outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    codex: Arc<CodexThread>,
) {
    let response = match approval_deadline {
        Some(deadline) => {
            tokio::select! {
                biased;
                response = receiver => match classify_approval_response(response, deadline) {
                    ApprovalResponse::OnTime(response) => response,
                    ApprovalResponse::TimedOut => {
                        outgoing.cancel_request(&elicitation_request_id).await;
                        let _ = codex
                            .submit(Op::ExecApproval {
                                id: approval_id,
                                turn_id: Some(event_id),
                                decision: ReviewDecision::TimedOut,
                            })
                            .await;
                        return;
                    }
                },
                () = tokio::time::sleep_until(deadline) => {
                    outgoing.cancel_request(&elicitation_request_id).await;
                    outgoing
                        .send_notification(crate::outgoing_message::OutgoingNotification {
                            method: "notifications/cancelled".to_string(),
                            params: Some(json!({
                                "requestId": elicitation_request_id,
                                "reason": "command approval expired",
                            })),
                        });
                    let _ = codex
                        .submit(Op::ExecApproval {
                            id: approval_id,
                            turn_id: Some(event_id),
                            decision: ReviewDecision::TimedOut,
                        })
                        .await;
                    return;
                }
            }
        }
        None => receiver.await.map(|response| response.result),
    };
    let value = match response {
        Ok(value) => value,
        Err(err) => {
            error!("request failed: {err:?}");
            return;
        }
    };

    // Try to deserialize `value` and then make the appropriate call to `codex`.
    let response = serde_json::from_value::<ExecApprovalResponse>(value).unwrap_or_else(|err| {
        error!("failed to deserialize ExecApprovalResponse: {err}");
        // If we cannot deserialize the response, we deny the request to be
        // conservative.
        ExecApprovalResponse {
            decision: ReviewDecision::denied("approval request failed"),
        }
    });

    if let Err(err) = codex
        .submit(Op::ExecApproval {
            id: approval_id,
            turn_id: Some(event_id),
            decision: response.decision,
        })
        .await
    {
        error!("failed to submit ExecApproval: {err}");
    }
}

enum ApprovalResponse {
    OnTime(Result<Value, tokio::sync::oneshot::error::RecvError>),
    TimedOut,
}

fn classify_approval_response(
    response: Result<
        crate::outgoing_message::ClientResponse,
        tokio::sync::oneshot::error::RecvError,
    >,
    deadline: tokio::time::Instant,
) -> ApprovalResponse {
    match response {
        Ok(response) if response.received_at < deadline => {
            ApprovalResponse::OnTime(Ok(response.result))
        }
        Ok(_) => ApprovalResponse::TimedOut,
        Err(err) => ApprovalResponse::OnTime(Err(err)),
    }
}

#[cfg(test)]
#[path = "exec_approval_tests.rs"]
mod tests;
