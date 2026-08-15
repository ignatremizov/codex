//! Live model-authored response-observation projection for the `/agent` picker.

use codex_app_server_protocol::AgentResponseHandling;

use super::agent_observation_display::AgentResponseObservationBinding;
use super::*;

impl App {
    /// Tracks live model-authored response observation without restoring transcript state.
    ///
    /// The picker cache is observer-relative because a sibling's subscription must not appear as
    /// the active thread's policy. Resume policies remain visible when the target is already
    /// terminal because they bind to its next admitted turn; spawn and send policies are consumed
    /// when their admitted target turn has already reached a final status.
    pub(super) fn cache_collab_response_observation_for_notification(
        &mut self,
        notification: &ServerNotification,
    ) {
        let ServerNotification::ItemCompleted(notification) = notification else {
            return;
        };
        let ThreadItem::CollabAgentToolCall {
            tool,
            status,
            observe_commentary,
            wake_on_completion,
            target_messages,
            queue_input,
            sender_thread_id,
            receiver_thread_ids,
            agents_states,
            ..
        } = &notification.item
        else {
            return;
        };
        let observer = ThreadId::from_string(sender_thread_id).ok();
        let response_handling = observe_commentary.map(|commentary| {
            let final_response = match wake_on_completion {
                Some(true) => codex_app_server_protocol::AgentFinalResponseHandling::Wake,
                Some(false) => codex_app_server_protocol::AgentFinalResponseHandling::Passive,
                None => codex_app_server_protocol::AgentFinalResponseHandling::Presentation,
            };
            AgentResponseHandling::new(
                commentary,
                final_response,
                target_messages.unwrap_or(false),
                /*queue_input*/ false,
            )
        });
        let records_observation = observe_commentary.is_some()
            && *queue_input != Some(true)
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
            if records_observation
                && (!target_is_final || resume_binds_next_turn)
                && let Some(observer) = observer
            {
                let binding = if target_is_final {
                    AgentResponseObservationBinding::NextTurn
                } else {
                    AgentResponseObservationBinding::Bound
                };
                self.agent_navigation.note_response_observation(
                    observer,
                    target,
                    binding,
                    response_handling,
                );
            }
            if target_is_final && !resume_binds_next_turn {
                self.agent_navigation.mark_stopped(target);
            }
        }
    }
}
