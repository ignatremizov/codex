//! Explicit durable unload; ordinary unsubscribe retains its idle grace period.

use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadUnloadParams;
use codex_app_server_protocol::ThreadUnloadResponse;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

struct PendingUnloadFence {
    pending: Arc<Mutex<HashSet<ThreadId>>>,
    ids: Vec<ThreadId>,
}

impl Drop for PendingUnloadFence {
    fn drop(&mut self) {
        let ids = std::mem::take(&mut self.ids);
        if ids.is_empty() {
            return;
        }
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            let mut pending = pending.lock().await;
            for id in ids {
                pending.remove(&id);
            }
        });
    }
}

impl ThreadRequestProcessor {
    pub(crate) async fn thread_unload(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadUnloadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        // The accepted request owns cleanup even if its connection or response waiter goes away.
        // Reuse the processor's existing background ownership, not a separate task registry.
        let processor = self.clone();
        self.background_tasks.spawn(async move {
            let result = processor.unload_subtree(&request_id, thread_id).await;
            match result {
                Ok(response) => processor.outgoing.send_response(request_id, response).await,
                Err(error) => processor.outgoing.send_error(request_id, error).await,
            }
        });
        Ok(None)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "exclusive subscriber preflight must share the listener subscription fence"
    )]
    async fn unload_subtree(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: ThreadId,
    ) -> Result<ThreadUnloadResponse, JSONRPCErrorError> {
        // Match resume/start ordering: list-state permit before the subscription fence.
        let _list_permit = self.acquire_thread_list_state_permit().await?;
        let mut subtree = self
            .thread_manager
            .prepare_subtree_unload(thread_id)
            .await
            .map_err(|error| match error.details() {
                CodexErrorDetails::ThreadNotFound(_) => {
                    invalid_request(format!("thread not found: {thread_id}"))
                }
                _ => internal_error(format!("prepare thread unload: {error}")),
            })?;
        let root_thread_id = subtree.root_thread_id();
        let mut fenced_ids = subtree.thread_ids().to_vec();
        if !fenced_ids.contains(&root_thread_id) {
            fenced_ids.push(root_thread_id);
        }
        {
            let mut pending = self.pending_thread_unloads.lock().await;
            for id in &fenced_ids {
                if pending.contains(id) {
                    return Err(invalid_request(format!(
                        "thread {id} is already unloading; retry after that operation completes"
                    )));
                }
                if self
                    .thread_state_manager
                    .subscribed_connection_ids(*id)
                    .await
                    .iter()
                    .any(|connection| *connection != request_id.connection_id)
                {
                    return Err(invalid_request(format!(
                        "thread {id} has another subscribed client; disconnect instead of unloading"
                    )));
                }
            }
            pending.extend(fenced_ids.iter().copied());
        }
        let mut fence = PendingUnloadFence {
            pending: Arc::clone(&self.pending_thread_unloads),
            ids: fenced_ids,
        };

        for id in subtree.thread_ids() {
            self.outgoing
                .cancel_requests_for_thread(*id, /*error*/ None)
                .await;
        }
        let result = subtree.shutdown_and_remove().await;
        if let Ok(unloaded) = &result {
            for id in unloaded {
                self.thread_state_manager.remove_thread_state(*id).await;
                self.thread_watch_manager
                    .remove_thread(&id.to_string())
                    .await;
                self.outgoing
                    .send_server_notification(ServerNotification::ThreadClosed(
                        ThreadClosedNotification {
                            thread_id: id.to_string(),
                        },
                    ))
                    .await;
            }
        }
        // Failure leaves every runtime entry in place with ordinary admission closed.
        // Clear the request fence so the same operation can retry the durable drain.
        {
            let mut pending = self.pending_thread_unloads.lock().await;
            for id in fence.ids.drain(..) {
                pending.remove(&id);
            }
        }
        result
            .map(|unloaded| ThreadUnloadResponse {
                root_thread_id: root_thread_id.to_string(),
                unloaded_thread_ids: unloaded.iter().map(ToString::to_string).collect(),
            })
            .map_err(|error| {
                internal_error(format!(
                    "thread unload did not complete; runtimes remain retained for retry: {error}"
                ))
            })
    }
}
