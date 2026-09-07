//! Admission-time authorization, independent of durable sender attribution.

use super::*;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl LocalAgentControl {
    pub(in crate::agent::control) fn target_message_wake_is_current(
        &self,
        wake: &crate::agent::turn_queue::QueuedTargetMessageWake,
    ) -> bool {
        let state = self.wait_agent_presentations.state();
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get(&(wake.observer, wake.target))
            .filter(|relationship| !relationship.revoked)
        else {
            return false;
        };
        let mode = relationship.reply_route.or_else(|| {
            state
                .inherited_message_routes
                .get(&(wake.observer, wake.target))
                .copied()
        });
        mode != Some(TargetMessageRouteMode::Disabled)
            && relationship
                .turns
                .get(&wake.target_turn_id)
                .is_some_and(|turn| {
                    turn.message_wake_reservation_id == Some(wake.reservation_id)
                        && (turn.target_messages || mode == Some(TargetMessageRouteMode::Enabled))
                })
    }

    pub(crate) async fn is_live_agent_descendant(
        &self,
        ancestor: ThreadId,
        descendant: ThreadId,
    ) -> CodexResult<bool> {
        let manager = self.upgrade()?;
        let mut cursor = descendant;
        let mut visited = HashSet::new();
        while visited.insert(cursor) {
            let target = manager.get_thread(cursor).await?;
            if target.session.services.agent_control.bound_session_id() != Some(self.session_id()) {
                return Ok(false);
            }
            let Some(parent) = target.session_source.parent_thread_id() else {
                return Ok(false);
            };
            if parent == ancestor {
                return Ok(true);
            }
            cursor = parent;
        }
        Ok(false)
    }

    /// The caller retains the permission transaction through exact SessionIo admission.
    /// Human input never enters this check, including previously accepted human queues.
    pub(in crate::agent::control) async fn ensure_model_input_authorized(
        &self,
        sender: SessionPresentationId,
        recipient: SessionPresentationId,
        sender_turn_id: &str,
    ) -> CodexResult<()> {
        let manager = self.upgrade()?;
        let source = manager.get_thread(sender.thread_id).await?;
        if source.session.presentation_id() != sender
            || source.session.services.agent_control.bound_session_id() != Some(self.session_id())
            || sender == recipient
        {
            return Err(CodexErr::InvalidRequest(
                "agent input source is no longer current".into(),
            ));
        }
        if self
            .is_live_agent_descendant(sender.thread_id, recipient.thread_id)
            .await?
        {
            return Ok(());
        }
        let snapshot = self.messaging_context_snapshot(sender).await?;
        if !snapshot.allowed_targets.contains(&recipient.thread_id) {
            return Err(CodexErr::InvalidRequest(
                "agent send permission ended before target-turn admission; input not submitted"
                    .into(),
            ));
        }
        let state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get(&(recipient, sender))
            .filter(|relationship| !relationship.revoked);
        let mode = relationship
            .and_then(|relationship| relationship.reply_route)
            .or_else(|| {
                state
                    .inherited_message_routes
                    .get(&(recipient, sender))
                    .copied()
            });
        let allowed = match mode {
            Some(TargetMessageRouteMode::Enabled) => true,
            Some(TargetMessageRouteMode::Disabled) => false,
            None => relationship.is_some_and(|relationship| {
                relationship
                    .turns
                    .get(sender_turn_id)
                    .is_some_and(|turn| turn.target_messages)
            }),
        };
        if allowed {
            Ok(())
        } else {
            Err(CodexErr::InvalidRequest(
                "agent send permission ended before target-turn admission; input not submitted"
                    .into(),
            ))
        }
    }
}
