//! Admission checks shared by explicit user-authored agent operations.

use super::*;

impl App {
    pub(super) async fn validated_agent_control_target(
        &mut self,
        app_server: &AppServerSession,
        source: ThreadId,
        selector: &crate::chatwidget::agent_command::AgentSelector,
    ) -> Result<String, String> {
        self.ensure_agent_control_admission(source, /*target*/ None)
            .map_err(|error| error.to_string())?;
        selector.control_target()?;
        let target = self.resolve_agent_selector(app_server, selector).await?;
        self.ensure_agent_control_admission(source, Some(target))
            .map_err(|error| error.to_string())?;
        Ok(target.to_string())
    }
    pub(super) fn recover_agent_command(
        &mut self,
        source: ThreadId,
        command: String,
        prompt: crate::chatwidget::UserMessage,
    ) {
        self.agent_command_recovery
            .entry(source)
            .or_default()
            .push((command, prompt));
        self.restore_agent_command_recovery();
    }

    pub(super) fn restore_agent_command_recovery(&mut self) {
        let Some(source) = self.chat_widget.thread_id() else {
            return;
        };
        if self.active_thread_id != Some(source) {
            return;
        }
        if let Some(commands) = self.agent_command_recovery.remove(&source) {
            for (command, prompt) in commands.into_iter().rev() {
                self.chat_widget.restore_user_message_to_composer(prompt);
                self.chat_widget
                    .restore_user_message_to_composer(command.into());
            }
        }
    }

    pub(super) fn ensure_agent_control_admission(
        &self,
        source: ThreadId,
        target: Option<ThreadId>,
    ) -> Result<()> {
        for thread_id in std::iter::once(source).chain(target) {
            if self.thread_unavailable(thread_id)
                || self
                    .thread_event_channels
                    .get(&thread_id)
                    .is_some_and(|channel| {
                        channel.attachment() == ThreadEventAttachment::ExternalWriter
                    })
            {
                color_eyre::eyre::bail!(
                    "Conversation {thread_id} is read-only or unavailable; no operation was sent"
                );
            }
        }
        if self.chat_widget.thread_id() == Some(source)
            && self.chat_widget.has_misalignment_policy_violation()
        {
            color_eyre::eyre::bail!(
                "This conversation is stopped as a precaution; no agent operation was sent"
            );
        }
        Ok(())
    }
}
