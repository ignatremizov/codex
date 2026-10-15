//! Typed control operation for resuming a turn without constructing user input.

use super::*;
use codex_app_server_protocol::TurnContinueParams;
use codex_app_server_protocol::TurnContinueResponse;

impl AppServerSession {
    pub(crate) async fn turn_continue(&mut self, thread_id: ThreadId) -> Result<()> {
        let request_id = self.next_request_id();
        let _: TurnContinueResponse = self
            .client
            .request_typed(ClientRequest::TurnContinue {
                request_id,
                params: TurnContinueParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await?;
        Ok(())
    }
}
