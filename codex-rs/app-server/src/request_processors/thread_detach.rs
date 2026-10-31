//! Explicit root promotion retires old subscriptions before the same UUID is resumed.

use super::thread_processor::ThreadRequestProcessor;
use super::thread_unload::PendingUnloadFence;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadDetachParams;
use codex_app_server_protocol::ThreadDetachResponse;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use std::sync::Arc;
use tokio::sync::watch;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_detach(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadDetachParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let processor = self.clone();
        self.background_tasks.spawn(async move {
            match processor.detach_idle_branch(thread_id).await {
                Ok(response) => processor.outgoing.send_response(request_id, response).await,
                Err(error) => processor.outgoing.send_error(request_id, error).await,
            }
        });
        Ok(None)
    }

    async fn detach_idle_branch(
        &self,
        thread_id: ThreadId,
    ) -> Result<ThreadDetachResponse, JSONRPCErrorError> {
        let _list_permit = self.acquire_thread_list_state_permit().await?;
        self.thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request("open the child conversation before detaching it"))?;
        let ids = self
            .thread_manager
            .list_agent_subtree_thread_ids(thread_id)
            .await
            .map_err(|error| internal_error(format!("read detach branch: {error}")))?;
        let mut loaded = Vec::new();
        for id in &ids {
            if self.thread_manager.get_thread(*id).await.is_ok() {
                loaded.push(*id);
            }
        }
        {
            let mut pending = self.pending_thread_unloads.lock().await;
            if ids.iter().any(|id| pending.contains_key(id)) {
                return Err(invalid_request(
                    "the branch is already changing ownership or unloading; retry when idle",
                ));
            }
            pending.extend(ids.iter().map(|id| (*id, watch::channel(()).0)));
        }
        let mut fence = PendingUnloadFence {
            pending: Arc::clone(&self.pending_thread_unloads),
            ids,
        };
        let result = self.thread_manager.detach_agent_branch(thread_id).await;
        if let Ok(detached) = &result {
            loaded.extend(detached.unloaded_thread_ids.iter().copied());
            loaded.sort_by_key(ToString::to_string);
            loaded.dedup();
        }
        // A failed graph commit can still follow an acknowledged unload. Retire those old
        // listeners as well; the next resume must read canonical ownership, not their cache.
        for id in loaded {
            if self.thread_manager.get_thread(id).await.is_err() {
                self.thread_state_manager.remove_thread_state(id).await;
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
        {
            let mut pending = self.pending_thread_unloads.lock().await;
            for id in fence.ids.drain(..) {
                pending.remove(&id);
            }
        }
        let detached = result.map_err(|error| match error.details() {
            CodexErrorDetails::InvalidRequest(message)
            | CodexErrorDetails::UnsupportedOperation(message) => invalid_request(message.clone()),
            _ => internal_error(format!(
                "detach failed; resume to reload current ownership: {error}"
            )),
        })?;
        Ok(ThreadDetachResponse {
            thread_id: thread_id.to_string(),
            previous_root_thread_id: detached.previous_root_thread_id.to_string(),
            unloaded_thread_ids: detached
                .unloaded_thread_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
    }
}
