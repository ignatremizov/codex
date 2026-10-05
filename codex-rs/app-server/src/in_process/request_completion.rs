//! Order history-mutation replies behind their preceding embedded-client events.

use super::InProcessServerEvent;
use super::PendingClientRequestResponse;
use codex_app_server_protocol::RequestId;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) struct PendingClientResponse {
    pub(super) response_tx: oneshot::Sender<PendingClientRequestResponse>,
    pub(super) ordered_boundary: Option<RequestId>,
}

impl PendingClientResponse {
    pub(super) async fn respond(
        self,
        result: PendingClientRequestResponse,
        events: &mpsc::Sender<InProcessServerEvent>,
    ) -> Result<(), mpsc::error::SendError<InProcessServerEvent>> {
        if let Some(request_id) = self.ordered_boundary {
            // The facade can resolve a reply on a different task from its event pump. Put the
            // boundary in that pump's FIFO, so consumers can wait for this exact request.
            // Losing the event stream must not turn an unobservable mutation into success.
            events
                .send(InProcessServerEvent::RequestCompleted { request_id })
                .await?;
        }
        let _ = self.response_tx.send(result);
        Ok(())
    }
}

#[cfg(test)]
#[path = "request_completion_tests.rs"]
mod tests;
