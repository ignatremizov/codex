//! Live wake-subscription projection for the `/agent` picker.

use super::*;

impl App {
    /// Tracks live V1 wake subscriptions without restoring them from saved transcript replay.
    ///
    /// The picker cache is observer-relative because a sibling's subscription must not appear as
    /// the active thread's subscription. Resume wakes remain visible when the target is already
    /// terminal because they bind to its next admitted turn; spawn and send wakes are consumed
    /// when their admitted target turn has already reached a final status.
    pub(super) fn cache_collab_wake_subscription_for_notification(
        &mut self,
        notification: &ServerNotification,
    ) {
        let ServerNotification::ItemCompleted(notification) = notification else {
            return;
        };
        let ThreadItem::CollabAgentToolCall {
            tool,
            status,
            wake_on_completion,
            sender_thread_id,
            receiver_thread_ids,
            agents_states,
            ..
        } = &notification.item
        else {
            return;
        };
        let observer = ThreadId::from_string(sender_thread_id).ok();
        let records_wake = *wake_on_completion == Some(true)
            && matches!(
                tool,
                codex_app_server_protocol::CollabAgentTool::SpawnAgent
                    | codex_app_server_protocol::CollabAgentTool::SendInput
                    | codex_app_server_protocol::CollabAgentTool::ResumeAgent
            )
            && matches!(
                status,
                codex_app_server_protocol::CollabAgentToolCallStatus::Completed
            );
        let resume_binds_next_turn = matches!(
            tool,
            codex_app_server_protocol::CollabAgentTool::ResumeAgent
        );

        for receiver_thread_id in receiver_thread_ids {
            let Ok(target) = ThreadId::from_string(receiver_thread_id) else {
                continue;
            };
            let target_is_final = agents_states.get(receiver_thread_id).is_some_and(|state| {
                matches!(
                    &state.status,
                    codex_app_server_protocol::CollabAgentStatus::Interrupted
                        | codex_app_server_protocol::CollabAgentStatus::Completed
                        | codex_app_server_protocol::CollabAgentStatus::Errored
                        | codex_app_server_protocol::CollabAgentStatus::Shutdown
                        | codex_app_server_protocol::CollabAgentStatus::NotFound
                )
            });
            if records_wake
                && (!target_is_final || resume_binds_next_turn)
                && let Some(observer) = observer
            {
                let binding = if target_is_final {
                    super::agent_navigation::WakeSubscriptionBinding::NextTurn
                } else {
                    super::agent_navigation::WakeSubscriptionBinding::Bound
                };
                self.agent_navigation
                    .note_wake_subscription(observer, target, binding);
            }
            if target_is_final && !resume_binds_next_turn {
                self.agent_navigation.mark_stopped(target);
            }
        }
    }
}
