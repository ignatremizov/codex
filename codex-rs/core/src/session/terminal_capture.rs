use super::*;
use crate::agent::control::AgentTerminalPresentation;
use crate::agent::control::TerminalPresentationDelivery;
use crate::agent::control::TerminalPresentationOutcome;

fn terminal_outcome(event: &EventMsg) -> Option<TerminalPresentationOutcome> {
    if let EventMsg::Error(error) = event {
        return agent_status_from_event(event).map(|status| TerminalPresentationOutcome {
            status,
            error_info: error.codex_error_info.clone(),
        });
    }
    if let EventMsg::TurnAborted(event) = event
        && event.reason == TurnAbortReason::Interrupted
        && let Some(error) = &event.error
        && error.codex_error_info == Some(CodexErrorInfo::TooManyDenials)
    {
        return Some(TerminalPresentationOutcome {
            status: AgentStatus::Errored(error.message.clone()),
            error_info: error.codex_error_info.clone(),
        });
    }
    agent_status_from_event(event).map(Into::into)
}

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
        if matches!(&status, AgentStatus::Running) {
            self.terminal_presentation_recorded
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if matches!(&status, AgentStatus::Running) || !is_final(&current_status) {
            self.agent_status.send_replace(status);
        }
    }

    fn record_sub_agent_terminal_presentation(
        &self,
        parent_thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        outcome: TerminalPresentationOutcome,
        delivery: TerminalPresentationDelivery,
    ) -> Option<AgentTerminalPresentation> {
        if matches!(&outcome.status, AgentStatus::Shutdown)
            && self.services.local_agent_runtime.shutdown.is_cancelled()
        {
            return None;
        }
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
        if is_final(&current_status)
            || self
                .terminal_presentation_recorded
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let child = self.presentation_id();
        let parent = self
            .services
            .local_agent_runtime
            .control(self.session_id())
            .completion_parent_for_child(child, parent_thread_id)?;
        let published_status = if outcome.error_info == Some(CodexErrorInfo::TooManyDenials) {
            AgentStatus::Interrupted
        } else {
            outcome.status.clone()
        };
        self.services
            .local_agent_runtime
            .control(self.session_id())
            .record_agent_terminal_presentation(parent, child, turn_id, outcome, delivery, || {
                self.terminal_presentation_recorded
                    .store(true, std::sync::atomic::Ordering::Release);
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
        let outcome = if delivery == TerminalPresentationDelivery::Direct {
            turn_context
                .terminal_error
                .lock()
                .await
                .as_ref()
                .map(|error| TerminalPresentationOutcome {
                    status: AgentStatus::Errored(error.message.clone()),
                    error_info: error.codex_error_info.clone(),
                })
                .or_else(|| terminal_outcome(event))?
        } else {
            terminal_outcome(event)?
        };
        if !is_final(&outcome.status) {
            return None;
        }
        self.record_sub_agent_terminal_presentation(
            *parent_thread_id,
            &turn_context.sub_id,
            outcome,
            delivery,
        )
    }

    pub(super) fn prepare_raw_sub_agent_terminal_presentation(&self, event: &Event) {
        let Some(outcome) =
            terminal_outcome(&event.msg).filter(|outcome| is_final(&outcome.status))
        else {
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
            outcome,
            TerminalPresentationDelivery::Watcher,
        );
    }
}
