//! Narrow, acknowledged updates to live reply permission, independent of accepted turn work.

use super::*;

#[cfg(test)]
#[path = "user_reply_route_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetMessageRouteMode {
    Enabled,
    Disabled,
}

impl TargetMessageRouteMode {
    pub(crate) fn is_enabled(self) -> bool {
        self == Self::Enabled
    }
}

pub(in crate::agent::control) struct PreparedReplyRoute {
    parent: SessionPresentationId,
    child: SessionPresentationId,
    pub(in crate::agent::control) previous: Option<TargetMessageRouteMode>,
    expected_context_installed: bool,
    mode: TargetMessageRouteMode,
    context_installed: bool,
    pub(in crate::agent::control) snapshots: Vec<AgentResponseObservation>,
}

impl LocalAgentControl {
    pub(crate) fn target_message_route_mode(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<TargetMessageRouteMode> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .filter(|relationship| !relationship.revoked)
            .and_then(|relationship| relationship.reply_route)
    }

    pub(in crate::agent::control) fn prepare_reply_route(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        mode: TargetMessageRouteMode,
    ) -> CodexResult<PreparedReplyRoute> {
        let state = self.wait_agent_presentations.state();
        let mut replacement = state
            .response_observation_by_observer_child
            .get(&(parent, child))
            .cloned()
            .unwrap_or_default();
        if replacement.revoked {
            return Err(CodexErr::InvalidRequest(
                "reply relationship is no longer live".into(),
            ));
        }
        let previous = replacement.reply_route;
        let expected_context_installed = replacement.reply_route_context_installed;
        replacement.reply_route = Some(mode);
        replacement.reply_route_context_installed |= mode.is_enabled();
        replacement.persistence = ResponseObservationPersistence::Durable;
        // Accepted queues/reservations and observation receipts are not revoked by this setting.
        let snapshots = super::snapshot::snapshots_for_relationship(parent, child, &replacement);
        Ok(PreparedReplyRoute {
            parent,
            child,
            previous,
            expected_context_installed,
            mode,
            context_installed: replacement.reply_route_context_installed,
            snapshots,
        })
    }

    pub(in crate::agent::control) fn commit_reply_route(
        &self,
        prepared: PreparedReplyRoute,
    ) -> CodexResult<()> {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((prepared.parent, prepared.child))
            .or_default();
        if relationship.revoked
            || relationship.reply_route != prepared.previous
            || relationship.reply_route_context_installed != prepared.expected_context_installed
        {
            return Err(CodexErr::Fatal(
                "reply route changed during canonical publication".into(),
            ));
        }
        relationship.persistence = ResponseObservationPersistence::Durable;
        relationship.reply_route = Some(prepared.mode);
        relationship.reply_route_context_installed = prepared.context_installed;
        drop(state);
        self.publish_response_observation_binding();
        Ok(())
    }
}
