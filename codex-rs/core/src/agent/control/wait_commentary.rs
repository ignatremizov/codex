//! Publish wait-owned commentary through the same exact-source canonical receipt boundary.

use super::presentation::WaitCommentaryDelivery;
use super::*;
use crate::session::turn_context::TurnContext;
use crate::session_prefix::format_subagent_commentary_message;

impl LocalAgentControl {
    pub(crate) async fn deliver_v1_wait_commentary(
        &self,
        parent: SessionPresentationId,
        turn: &Arc<TurnContext>,
        commentary: &WaitCommentaryDelivery,
    ) -> bool {
        let result = async {
            let state = self.upgrade()?;
            let observer = state.get_thread(parent.thread_id).await?;
            if observer.session.presentation_id() != parent {
                return Err(CodexErr::ThreadNotFound(parent.thread_id));
            }
            let accepted = observer
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("wait observer is closing".into()))?;
            let agent = self
                .model_visible_agent_identity_for_version(
                    observer
                        .multi_agent_version()
                        .unwrap_or(MultiAgentVersion::V1),
                    commentary.child.thread_id,
                )
                .await?;
            let mut communication = InterAgentCommunication::new(
                self.get_agent_metadata(commentary.child.thread_id)
                    .and_then(|metadata| metadata.agent_path)
                    .unwrap_or_else(AgentPath::root),
                observer
                    .session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root),
                Vec::new(),
                format_subagent_commentary_message(
                    agent,
                    &commentary.turn_id,
                    &commentary.delivery.source_item_id,
                    &commentary.delivery.text,
                ),
                /*trigger_turn*/ false,
            );
            communication.id = Some(commentary.delivery.response_item_id.clone());
            let commit = ResponseObservationDeliveryCommit {
                parent,
                child: commentary.child,
                turn_id: commentary.turn_id.clone(),
                response_item_id: commentary.delivery.response_item_id.clone(),
                kind: ResponseObservationDeliveryKind::Commentary,
                mailbox_final_subscription_message_id: None,
                model_visibility:
                    codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
            };
            observer
                .session
                .record_wait_commentary(Arc::clone(turn), communication, commit, accepted)
                .await
        }
        .await;
        if let Err(error) = result {
            warn!("wait commentary publication did not acknowledge delivery: {error}");
            false
        } else {
            true
        }
    }
}
