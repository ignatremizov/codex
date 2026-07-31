//! Lazy revival of replay-only threads before live thread operations.
//!
//! Selecting a closed subagent intentionally remains a read-only transcript operation. The first
//! operation that needs a live thread revives the persisted app-server thread, updates the existing
//! replay channel in place, and then lets normal thread routing submit the preserved operation.

use super::*;
use crate::chatwidget::ThreadInputStateRestoreMode;

impl App {
    /// Only live commands revive a closed thread; observation and stale replies never do.
    pub(super) async fn prepare_replay_only_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> bool {
        let needs_live_thread = match op {
            AppCommand::UserTurn { .. }
            | AppCommand::Compact
            | AppCommand::Review { .. }
            | AppCommand::RunUserShellCommand { .. }
            | AppCommand::ActivateMcpServer { .. }
            | AppCommand::CleanBackgroundTerminals
            | AppCommand::OverrideTurnContext { .. }
            | AppCommand::ApproveGuardianDeniedAction { .. }
            | AppCommand::Interrupt => true,
            AppCommand::RealtimeConversationStart {
                thread_id: realtime_thread_id,
                ..
            }
            | AppCommand::RealtimeConversationStop {
                thread_id: realtime_thread_id,
            } => *realtime_thread_id == thread_id,
            AppCommand::RealtimeConversationSpeech {
                thread_id: realtime_thread_id,
                attempt_id,
                input_generation,
                delivery_id,
                ..
            } => {
                *realtime_thread_id == thread_id
                    && self.chat_widget.has_pending_realtime_speech(*delivery_id)
                    && self.chat_widget.is_current_realtime_attempt(
                        *realtime_thread_id,
                        *attempt_id,
                        *input_generation,
                    )
            }
            AppCommand::ListSkills { .. }
            | AppCommand::ReloadUserConfig
            | AppCommand::SetThreadName { .. }
            | AppCommand::ExecApproval { .. }
            | AppCommand::PatchApproval { .. }
            | AppCommand::ResolveElicitation { .. }
            | AppCommand::ResolveUserVerification { .. }
            | AppCommand::UserInputAnswer { .. }
            | AppCommand::RequestPermissionsResponse { .. } => false,
        };
        if !needs_live_thread {
            return true;
        }
        if let Err(error) = self.resume_replay_only_thread(app_server, thread_id).await {
            if let AppCommand::RealtimeConversationSpeech { delivery_id, .. } = op {
                self.chat_widget
                    .restore_undelivered_realtime_speech(*delivery_id);
            }
            if let AppCommand::RealtimeConversationStart { thread_id, .. }
            | AppCommand::RealtimeConversationStop { thread_id } = op
                && self.chat_widget.thread_id() == Some(*thread_id)
            {
                self.chat_widget.record_realtime_failure();
                self.chat_widget.reset_realtime_conversation();
            }
            if self.chat_widget.thread_id() == Some(thread_id) {
                // Preserve pending prompts and attachments in the existing manual-recovery
                // queue. Do not drain the next queued prompt into another failed resume.
                self.chat_widget.pause_unavailable_thread();
            }
            self.chat_widget.add_error_message(format!(
                "Failed to resume agent thread: {error:#}. No operation was sent. Your input is preserved; reopen the parent conversation and retry."
            ));
            return false;
        }
        true
    }

    pub(super) async fn resume_replay_only_thread(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        if self.thread_unavailable(thread_id)
            || self
                .thread_event_channels
                .get(&thread_id)
                .is_some_and(|channel| {
                    channel.attachment() == ThreadEventAttachment::ExternalWriter
                })
        {
            color_eyre::eyre::bail!("this conversation is read-only or unavailable");
        }
        let is_replay_only_channel = self
            .thread_event_channels
            .get(&thread_id)
            .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::ReplayOnly);
        let is_closed_agent = self
            .agent_navigation
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed);
        if !is_replay_only_channel && !is_closed_agent {
            return Ok(());
        }

        let AppServerStartedThread {
            session,
            turns,
            is_subagent,
            ..
        } = app_server
            .resume_thread(
                &self.local_settings,
                self.config.clone(),
                thread_id,
                crate::app_server_session::ResumeModelSettings::PreserveExistingThread,
            )
            .await?;
        if is_subagent {
            self.agent_navigation.mark_subagent(thread_id);
        }
        let is_running = turns
            .last()
            .is_some_and(|turn| matches!(turn.status, TurnStatus::InProgress));

        let is_active = self.active_thread_id == Some(thread_id);
        let attach_active_receiver = is_active && self.active_thread_rx.is_none();
        let replacement_receiver = {
            let channel = self.ensure_thread_channel(thread_id);
            channel.mark_live();
            {
                let mut store = channel.store.lock().await;
                store.set_session(session.clone(), turns);
                store.rebase_buffer_after_session_refresh();
                if is_active {
                    store.active = true;
                }
            }
            if attach_active_receiver {
                channel.receiver.take()
            } else {
                None
            }
        };
        if let Some(receiver) = replacement_receiver {
            self.active_thread_rx = Some(receiver);
        }

        if self.active_thread_id == Some(thread_id)
            && self.chat_widget.thread_id() == Some(thread_id)
        {
            let input = self.chat_widget.capture_thread_input_state();
            self.chat_widget.handle_thread_session_quiet(session);
            self.chat_widget.restore_thread_input_state(
                input,
                ThreadInputStateRestoreMode {
                    preserve_in_flight_turn: true,
                },
            );
        }

        if let Some(entry) = self.agent_navigation.get(&thread_id).cloned() {
            self.upsert_agent_picker_thread(
                thread_id,
                entry.agent_nickname,
                entry.agent_role,
                /*is_closed*/ false,
            );
            self.agent_navigation.set_running(thread_id, is_running);
            self.sync_active_agent_label();
        }

        Ok(())
    }
}
