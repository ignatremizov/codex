//! Explicit MCP activation is request admission, not a new sampling turn.

use super::*;
use codex_app_server_protocol::ThreadMcpServerActivateOutcome;
use codex_app_server_protocol::ThreadMcpServerActivateParams;
use codex_app_server_protocol::ThreadMcpServerActivateResponse;

impl AppServerSession {
    pub(crate) async fn thread_mcp_server_activate(
        &mut self,
        thread_id: ThreadId,
        server_name: String,
    ) -> Result<ThreadMcpServerActivateOutcome> {
        let request_id = self.next_request_id();
        let response: ThreadMcpServerActivateResponse = self
            .client
            .request_typed(ClientRequest::ThreadMcpServerActivate {
                request_id,
                params: ThreadMcpServerActivateParams {
                    thread_id: thread_id.to_string(),
                    server_name,
                },
            })
            .await
            .wrap_err("thread/mcpServer/activate failed")?;
        Ok(response.outcome)
    }
}
