//! Graceful quit stops the owning task; explicit disconnect only detaches the client.

use super::*;

impl App {
    pub(super) async fn handle_exit_mode(
        &mut self,
        app_server: &mut AppServerSession,
        mode: ExitMode,
    ) -> AppRunControl {
        if mode == ExitMode::Disconnect && app_server.uses_embedded_app_server() {
            self.chat_widget.add_error_message(
                "Cannot disconnect: this TUI owns its embedded server, so exiting cannot leave work running. Use /quit to stop and unload the task."
                    .to_string(),
            );
            return AppRunControl::Continue;
        }
        if matches!(
            mode,
            ExitMode::ShutdownFirst | ExitMode::ShutdownAfterInterrupt
        ) {
            let root = self
                .agent_root_thread_id()
                .or(self.primary_thread_id)
                .or(self.active_thread_id)
                .or(self.chat_widget.thread_id());
            self.pending_shutdown_exit_thread_id = root;
            if let Some(root) = root
                && let Err(error) = app_server.thread_unload(root).await
            {
                self.pending_shutdown_exit_thread_id = None;
                self.chat_widget.restore_composer_after_failed_shutdown();
                self.chat_widget.add_error_message(format!(
                    "Could not unload this task: {error:#}. Some runtimes may already be stopped and retained for retry. Retry /quit, or use /disconnect with a shared server."
                ));
                return AppRunControl::Continue;
            }
        }

        // A denied unload/disconnect must not cancel client-owned tools. Once the server
        // acknowledges shutdown, or explicit detach is accepted, clean up their local tasks.
        for (request_id, (_, task)) in self.dynamic_tool_tasks.drain() {
            task.abort();
            let response = crate::dynamic_tools::failure_response(
                "TUI disconnected while handling a dynamic tool call",
            );
            match serde_json::to_value(response) {
                Ok(result) => {
                    if let Err(error) = app_server
                        .resolve_server_request(request_id.clone(), result)
                        .await
                    {
                        tracing::warn!(?request_id, %error, "failed to cancel dynamic tool call");
                    }
                }
                Err(error) => {
                    tracing::warn!(?request_id, %error, "failed to serialize dynamic tool response");
                }
            }
        }
        self.pending_shutdown_exit_thread_id = None;
        AppRunControl::Exit(match mode {
            ExitMode::ShutdownFirst => ExitReason::UserRequested,
            ExitMode::ShutdownAfterInterrupt => ExitReason::TurnInterrupted,
            ExitMode::Disconnect | ExitMode::Immediate => ExitReason::Disconnected,
        })
    }
}
