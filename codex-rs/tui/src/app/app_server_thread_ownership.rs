//! Ownership checks for routing app-server events to a TUI root.

use super::App;
use crate::app_server_session::AppServerSession;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_protocol::ThreadId;
use std::time::Duration;

async fn read_subagent_parent_thread_id(
    app_server_client: &AppServerSession,
    thread_id: ThreadId,
) -> Option<ThreadId> {
    let response = tokio::time::timeout(Duration::from_secs(/*secs*/ 1), async {
        let mut retry_delay = Duration::from_millis(50);
        loop {
            match app_server_client
                .request_handle()
                .request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                    request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                    params: ThreadReadParams {
                        thread_id: thread_id.to_string(),
                        include_turns: false,
                    },
                })
                .await
            {
                Ok(response) => return Some(response),
                Err(TypedRequestError::Server { source, .. })
                    if source.message.starts_with("thread not found:")
                        || source.message.starts_with("thread not loaded:") =>
                {
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(1));
                }
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()??;
    crate::app_server_session::thread_parent_thread_id(&response.thread)
}

impl App {
    pub(super) fn owns_thread_for_routing(&self, thread_id: ThreadId) -> bool {
        self.primary_thread_id == Some(thread_id)
            || self.thread_event_channels.contains_key(&thread_id)
            || self.side_threads.contains_key(&thread_id)
            || self.agent_navigation.get(&thread_id).is_some()
    }

    pub(super) async fn owns_untracked_notification(
        &mut self,
        app_server_client: &AppServerSession,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) -> bool {
        let parent_thread_id = match notification {
            ServerNotification::ThreadStarted(started) => {
                crate::app_server_session::thread_parent_thread_id(&started.thread)
            }
            ServerNotification::McpServerStatusUpdated(_) => {
                read_subagent_parent_thread_id(app_server_client, thread_id).await
            }
            ServerNotification::ItemCompleted(_)
                if super::agent_questions::is_native_async_question(notification) =>
            {
                // A fast child can ask before its parent's spawn result reaches this client.
                let parent = read_subagent_parent_thread_id(app_server_client, thread_id).await;
                if let Some(parent) = parent.filter(|parent| self.owns_thread_for_routing(*parent))
                {
                    self.agent_navigation.mark_subagent(thread_id);
                    self.agent_navigation
                        .set_parent_thread_id(thread_id, Some(parent));
                    self.agent_navigation.upsert(
                        thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                        /*is_closed*/ false,
                    );
                }
                parent
            }
            _ => None,
        };
        parent_thread_id.is_some_and(|thread_id| self.owns_thread_for_routing(thread_id))
    }
}
