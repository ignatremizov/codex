//! Close replay is a one-attempt publication owned by the exact source runtime.

use super::*;
use crate::context::AgentContextIdentity;
use crate::session::AcceptedCompletionDelivery;
use crate::session::session::Session;
use crate::session_prefix::format_subagent_notification_message;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item_with_visibility;

/// The source capability is acquired before closing the target, not looked up afterward.
pub(crate) struct CloseAgentResponseContext {
    source_session: Arc<Session>,
    accepted: AcceptedCompletionDelivery,
    source: AgentPath,
    target: AgentPath,
    agent: AgentContextIdentity,
    target_thread_id: ThreadId,
    context_id: codex_protocol::ResponseItemId,
    terminal: Option<(SessionPresentationId, String, AgentTerminalPresentation)>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseAgentResponseDisposition {
    NotApplicable,
    Suppressed,
    AlreadyVisible,
    DeliveryPending,
    Delivered,
    Queued,
    PresentationOnly,
}

/// Status captured inside the target lifecycle boundary.
#[derive(Clone)]
pub(crate) struct ClosedAgent {
    pub(crate) previous_status: AgentStatus,
    pub(crate) previous_presentation: Option<SessionPresentationId>,
    pub(crate) previous_turn_id: Option<String>,
}

impl LocalAgentControl {
    pub(crate) async fn prepare_close_agent_response(
        &self,
        source_session: Arc<Session>,
        source_multi_agent_version: MultiAgentVersion,
        target_thread_id: ThreadId,
    ) -> CodexResult<CloseAgentResponseContext> {
        let state = self.upgrade()?;
        let source_thread = state.get_thread(source_session.thread_id()).await?;
        if source_thread.session.presentation_id() != source_session.presentation_id() {
            return Err(CodexErr::ThreadNotFound(source_session.thread_id()));
        }
        if target_thread_id == source_session.thread_id()
            || self
                .live_thread_spawn_descendants(target_thread_id)
                .await?
                .contains(&source_session.thread_id())
        {
            return Err(CodexErr::InvalidRequest(
                "a close response cannot target its source's own subtree".into(),
            ));
        }
        let accepted = source_session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("close response source is closing".into()))?;
        let agent = self
            .model_visible_agent_identity_for_version(source_multi_agent_version, target_thread_id)
            .await?;
        let target = self
            .get_agent_metadata(target_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .unwrap_or_else(AgentPath::root);
        let source = source_thread
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let terminal =
            match state.get_thread(target_thread_id).await {
                Ok(target) => {
                    let child = target.session.presentation_id();
                    let (snapshot, _) = target.session.subscribe_agent_responses();
                    snapshot.last_terminal.and_then(|(turn_id, _)| {
                        let model_delivery = self
                            .response_observation_snapshots(source_session.presentation_id(), child)
                            .into_iter()
                            .find(|observation| {
                                observation.target_turn_id.as_deref() == Some(turn_id.as_str())
                            })
                            .is_none_or(|observation| {
                                matches!(observation.final_delivery,
                            codex_protocol::protocol::AgentResponseFinalDelivery::Passive
                            | codex_protocol::protocol::AgentResponseFinalDelivery::Wake)
                            });
                        if !model_delivery {
                            return None;
                        }
                        self.response_observation_terminal(
                            source_session.presentation_id(),
                            child,
                            &turn_id,
                        )
                        .map(|terminal| (child, turn_id, terminal))
                    })
                }
                Err(_) => None,
            };
        Ok(CloseAgentResponseContext {
            source_session,
            accepted,
            source,
            target,
            agent,
            target_thread_id,
            context_id: new_sub_agent_completion_context_response_item_id(),
            terminal,
        })
    }
}

impl CloseAgentResponseContext {
    pub(crate) async fn deliver(
        self,
        closed: &ClosedAgent,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<CloseAgentResponseDisposition> {
        let closed = closed.clone();
        // Closing has already committed. Cancellation cannot abandon publication after enqueue.
        tokio::spawn(async move {
            let status = closed.previous_status;
            if !matches!(&status, AgentStatus::Completed(Some(message)) if !message.is_empty()) {
                return Ok(CloseAgentResponseDisposition::NotApplicable);
            }
            if policy.final_response() == FinalResponseObservation::None {
                return Ok(CloseAgentResponseDisposition::Suppressed);
            }
            let terminal = self.terminal.as_ref().filter(|(child, turn_id, _)| {
                closed.previous_presentation == Some(*child)
                    && closed.previous_turn_id.as_deref() == Some(turn_id.as_str())
            }).map(|(_, _, terminal)| terminal);
            if let Some(terminal) = terminal {
                match self.source_session.completion_context_state(&terminal.completion_context_response_item_id()).await? {
                    crate::session::CompletionContextState::Present => {
                        return Ok(CloseAgentResponseDisposition::AlreadyVisible);
                    }
                    crate::session::CompletionContextState::Unacknowledged => {
                        // Missing history alone never authorizes retrying an accepted receipt.
                        return Ok(CloseAgentResponseDisposition::DeliveryPending);
                    }
                    crate::session::CompletionContextState::SettledRemoved => {
                        // Positive receipt proof permits a new explicitly requested replay
                        // after replacement history removed the earlier model context.
                    }
                }
            }
            let hidden = policy.final_response() == FinalResponseObservation::PresentationOnly;
            let visibility = if hidden {
                SubAgentCompletionModelVisibility::NotVisible
            } else {
                SubAgentCompletionModelVisibility::Visible
            };
            let item = sub_agent_completion_item_with_visibility(
                &self.target_thread_id.to_string(), &status, visibility,
            ).ok_or_else(|| CodexErr::InvalidRequest("completed agent has no presentation".into()))?;
            let presentation = CompletionPresentation {
                item: TurnItem::AgentMessage(item),
                history_only_turn_id: Uuid::now_v7().to_string(),
            };
            if hidden {
                self.source_session.publish_completion_item(&presentation, &self.accepted).await?;
                return Ok(CloseAgentResponseDisposition::PresentationOnly);
            }
            let exec = self.source_session.app_server_client_metadata().await.client_name.as_deref() == Some("codex_exec");
            let queued = policy.queue_input() && !exec;
            let mut communication = InterAgentCommunication::new(
                self.target, self.source, Vec::new(),
                format_subagent_notification_message(self.agent, &status),
                queued || policy.final_response() == FinalResponseObservation::Wake && !exec,
            );
            communication.id = Some(self.context_id);
            communication.defer_to_next_turn = queued;
            self.source_session.publish_close_response(communication, presentation, self.accepted).await?;
            Ok(if queued { CloseAgentResponseDisposition::Queued } else { CloseAgentResponseDisposition::Delivered })
        }).await.map_err(|error| CodexErr::Fatal(format!("agent was closed; close response outcome unknown: {error}; do not replay automatically")))?
    }
}
