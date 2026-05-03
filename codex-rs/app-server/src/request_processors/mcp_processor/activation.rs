//! Thread-scoped admission for explicit MCP inventory in model context.

use super::McpRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadMcpServerActivateOutcome;
use codex_app_server_protocol::ThreadMcpServerActivateParams;
use codex_app_server_protocol::ThreadMcpServerActivateResponse;
use codex_protocol::protocol::Op;

impl McpRequestProcessor {
    pub(crate) async fn thread_mcp_server_activate(
        &self,
        params: ThreadMcpServerActivateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let ThreadMcpServerActivateParams {
            thread_id,
            server_name,
        } = params;
        let (_, thread) = self.load_thread(&thread_id).await?;
        let (mcp_config, _) = thread.current_mcp_config_and_runtime_context().await;
        let configured_servers = codex_mcp::configured_mcp_servers(&mcp_config);
        let Some(server_config) = configured_servers.get(&server_name) else {
            return Err(invalid_request(format!(
                "unknown MCP server `{server_name}` for thread {thread_id}"
            )));
        };
        if !server_config.enabled {
            return Err(invalid_request(format!(
                "MCP server `{server_name}` is disabled for thread {thread_id}"
            )));
        }

        let outcome = if thread
            .mcp_server_would_be_direct_at_session_start(&server_name)
            .await
        {
            ThreadMcpServerActivateOutcome::AlreadyImplicitlyAvailable
        } else if thread
            .latest_mcp_server_use_context_text(&server_name)
            .await
            .is_some()
        {
            ThreadMcpServerActivateOutcome::AlreadyActivated
        } else {
            thread
                .submit(Op::ActivateMcpServer {
                    server_name: server_name.clone(),
                })
                .await
                .map_err(|err| {
                    internal_error(format!(
                        "failed to queue MCP server `{server_name}` activation: {err}"
                    ))
                })?;
            // Submission is admission, not proof that the actor has captured or persisted tools.
            ThreadMcpServerActivateOutcome::Activated
        };
        Ok(Some(ThreadMcpServerActivateResponse { outcome }.into()))
    }
}
