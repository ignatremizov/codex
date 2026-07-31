//! Lazy revival of replay-only threads before live thread operations.
//!
//! Selecting a closed subagent intentionally remains a read-only transcript operation. The first
//! operation that needs a live thread revives the persisted app-server thread, updates the existing
//! replay channel in place, and then lets normal thread routing submit the preserved operation.

use super::*;

impl App {
    pub(super) async fn resume_replay_only_thread(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
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

        let AppServerStartedThread { session, turns } = app_server
            .resume_thread(self.config.clone(), thread_id, self.resume_model_settings())
            .await?;
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
            self.chat_widget.handle_thread_session_quiet(session);
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
