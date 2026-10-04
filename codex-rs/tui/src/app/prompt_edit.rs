//! Source-preserving prompt edits after authoritative history selection.
//!
//! The default in-place revert and its reconciliation remain in the event dispatcher. This
//! opt-in path uses the ordinary fork/attach owners, without submitting the restored draft.

use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::app_server_session::ForkGoalContinuation;
use crate::chatwidget::UserMessage;

impl App {
    pub(super) async fn fork_for_prompt_edit(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        before_turn_id: String,
        prompt: UserMessage,
    ) {
        if self.active_thread_id != Some(thread_id)
            || self.chat_widget.side_conversation_active()
            || self.chat_widget.is_external_writer_view()
        {
            self.restore_backtrack_prompt_after_revert_error(
                prompt,
                "the selected thread is no longer available for editing",
            );
            tui.frame_requester().schedule_frame();
            return;
        }
        self.refresh_in_memory_config_from_disk_best_effort("forking before the selected prompt")
            .await;
        let mut config = self.config.clone();
        if app_server.uses_remote_workspace() {
            config
                .workspace_roots
                .clone_from(&self.chat_widget.config_ref().workspace_roots);
        }
        config.model = Some(self.chat_widget.current_model().to_string());
        config.model_reasoning_effort = self.chat_widget.current_reasoning_effort();
        let selected_profile = self.confirmed_server_profile(thread_id);
        let forked = match app_server
            .fork_thread_at(
                &self.local_settings,
                config,
                thread_id,
                /*last_turn_id*/ None,
                /*before_turn_id*/ Some(before_turn_id),
                ForkGoalContinuation::DeferUntilNextTurn,
                selected_profile.as_ref(),
            )
            .await
        {
            Ok(forked) => forked,
            Err(err) => {
                self.restore_backtrack_prompt_after_revert_error(prompt, err);
                tui.frame_requester().schedule_frame();
                return;
            }
        };
        let fork_id = forked.session.thread_id;
        self.session_telemetry.counter(
            "codex.thread.fork",
            /*inc*/ 1,
            &[("source", "transcript")],
        );
        // Detach from the source only after the new thread exists. This does not unload or
        // rewrite the source, and an attach failure must never trigger another fork request.
        self.shutdown_current_thread(app_server).await;
        let attached = self
            .replace_chat_widget_with_app_server_thread(
                tui,
                forked,
                ThreadAttachPresentation::SessionLineage,
                /*initial_user_message*/ None,
            )
            .await;
        self.chat_widget.restore_user_message_to_composer(prompt);
        if let Err(err) = attached {
            self.chat_widget.add_error_message(format!(
                "Created fork {fork_id}, but could not attach to it: {err:#}. Your draft is restored; resume {fork_id} to continue. The source conversation is unchanged."
            ));
        }
        tui.frame_requester().schedule_frame();
    }
}
