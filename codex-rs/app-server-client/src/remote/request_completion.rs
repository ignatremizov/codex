//! Correlate rollback replies with the remote reader's ordered event queue.

use crate::AppServerEvent;
use crate::RequestResult;
use codex_app_server_protocol::RequestId;
use std::io::Result as IoResult;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) struct PendingResponse {
    pub(super) response_tx: oneshot::Sender<IoResult<RequestResult>>,
    pub(super) ordered_boundary: Option<RequestId>,
}

impl PendingResponse {
    pub(super) fn respond(
        self,
        result: RequestResult,
        events: &mpsc::UnboundedSender<AppServerEvent>,
    ) -> IoResult<()> {
        if let Some(request_id) = self.ordered_boundary {
            super::deliver_event(events, AppServerEvent::RequestCompleted { request_id })?;
        }
        let _ = self.response_tx.send(Ok(result));
        Ok(())
    }
}

#[cfg(test)]
#[path = "request_completion_tests.rs"]
mod tests;
