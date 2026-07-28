use super::*;
use crate::agent::control::AgentTerminalPresentation;
use crate::agent::control::TerminalPresentationDelivery;

impl Session {
    pub(super) fn publish_agent_status_from_event(&self, event: &EventMsg) {
        let Some(status) = agent_status_from_event(event) else {
            return;
        };
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_status = self.agent_status.borrow().clone();
        if matches!(&status, AgentStatus::Running) || !is_final(&current_status) {
            self.agent_status.send_replace(status);
        }
    }

    fn record_sub_agent_terminal_presentation(
        &self,
        parent_thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        status: AgentStatus,
        delivery: TerminalPresentationDelivery,
    ) -> Option<AgentTerminalPresentation> {
        if !self
            .terminal_presentation_armed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_status = self.agent_status.borrow().clone();
        if is_final(&current_status) {
            return None;
        }
        let child = self.presentation_id();
        let parent = self
            .services
            .agent_control
            .completion_parent_for_child(child, parent_thread_id)?;
        let published_status = status.clone();
        self.services
            .agent_control
            .record_agent_terminal_presentation(parent, child, turn_id, status, delivery, || {
                self.agent_status.send_replace(published_status);
            })
    }

    pub(super) async fn prepare_sub_agent_terminal_presentation(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) -> Option<AgentTerminalPresentation> {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path,
            ..
        }) = &turn_context.session_source
        else {
            return None;
        };
        let delivery = match (turn_context.multi_agent_version, event) {
            (
                codex_protocol::protocol::MultiAgentVersion::V2,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_),
            ) if agent_path.is_some() => TerminalPresentationDelivery::Direct,
            _ => TerminalPresentationDelivery::Watcher,
        };
        let status = if delivery == TerminalPresentationDelivery::Direct {
            turn_context
                .terminal_error
                .lock()
                .await
                .as_ref()
                .map(|error| AgentStatus::Errored(error.message.clone()))
                .or_else(|| agent_status_from_event(event))?
        } else {
            agent_status_from_event(event)?
        };
        if !is_final(&status) {
            return None;
        }
        self.record_sub_agent_terminal_presentation(
            *parent_thread_id,
            &turn_context.sub_id,
            status,
            delivery,
        )
    }

    pub(super) fn prepare_raw_sub_agent_terminal_presentation(&self, event: &Event) {
        let Some(status) = agent_status_from_event(&event.msg).filter(is_final) else {
            return;
        };
        let Some(parent_thread_id) = self.spawn_parent_thread_id else {
            return;
        };
        let generated_turn_id;
        let turn_id = if event.id.is_empty() {
            generated_turn_id = uuid::Uuid::now_v7().to_string();
            generated_turn_id.as_str()
        } else {
            event.id.as_str()
        };
        let _ = self.record_sub_agent_terminal_presentation(
            parent_thread_id,
            turn_id,
            status,
            TerminalPresentationDelivery::Watcher,
        );
    }
}
