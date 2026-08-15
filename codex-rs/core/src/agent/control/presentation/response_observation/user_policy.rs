//! Prepared user policy changes. Canonical acknowledgment precedes registry publication.

use super::*;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplacedFinalResponseObservationBinding {
    ActiveTurn,
    NextTurn,
    UndeliveredCompletion,
}

pub(in crate::agent::control) struct PreparedUserObservation {
    parent: SessionPresentationId,
    child: SessionPresentationId,
    turn_id: Option<String>,
    expected: ResponseTurnObservation,
    replacement: ResponseTurnObservation,
    pub(in crate::agent::control) snapshots: Vec<AgentResponseObservation>,
}

impl LocalAgentControl {
    pub(in crate::agent::control) fn reserved_response_observation_policy(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<ResponseObservationPolicy> {
        let state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get(&(parent, child))?;
        if relationship.revoked {
            return None;
        }
        let pending = relationship.pending_next_turn.as_ref()?;
        Some(ResponseObservationPolicy::from_parts(
            !pending.commentary_admissions.is_empty(),
            pending.final_response,
        ))
    }

    pub(in crate::agent::control) fn user_observation_binding(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        active: Option<&str>,
        terminal: Option<&str>,
    ) -> Option<(ReplacedFinalResponseObservationBinding, Option<String>)> {
        let state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get(&(parent, child))?;
        if relationship.revoked {
            return None;
        }
        if let Some(turn) = active.filter(|turn| relationship.turns.contains_key(*turn)) {
            return Some((
                ReplacedFinalResponseObservationBinding::ActiveTurn,
                Some(turn.to_owned()),
            ));
        }
        if let Some(turn) = terminal.filter(|turn| {
            relationship.turns.get(*turn).is_some_and(|observation| {
                observation.final_response != FinalResponseObservation::None
            })
        }) {
            return Some((
                ReplacedFinalResponseObservationBinding::UndeliveredCompletion,
                Some(turn.to_owned()),
            ));
        }
        relationship
            .pending_next_turn
            .as_ref()
            .map(|_| (ReplacedFinalResponseObservationBinding::NextTurn, None))
    }

    pub(in crate::agent::control) fn user_observation_task(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: Option<&str>,
    ) -> Option<(Option<String>, bool, FinalResponseObservation)> {
        let state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get(&(parent, child))?;
        let observation = match turn_id {
            Some(turn) => relationship.turns.get(turn),
            None => relationship.pending_next_turn.as_ref(),
        }?;
        Some((
            observation.task_preview.clone(),
            observation.promoted_task_context.is_some(),
            observation.final_response,
        ))
    }

    /// Prepare a narrow compare-and-install patch, retaining terminal receipt identity.
    pub(in crate::agent::control) fn prepare_user_observation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: Option<String>,
        final_response: Option<FinalResponseObservation>,
        task_preview: Option<String>,
        task: Option<&ResponseItem>,
    ) -> CodexResult<PreparedUserObservation> {
        let state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get(&(parent, child))
            .filter(|relationship| !relationship.revoked)
            .ok_or_else(|| {
                CodexErr::InvalidRequest("response observation is no longer live".into())
            })?;
        let expected = match turn_id.as_deref() {
            Some(turn) => relationship.turns.get(turn),
            None => relationship.pending_next_turn.as_ref(),
        }
        .cloned()
        .ok_or_else(|| CodexErr::InvalidRequest("no pending response observation".into()))?;
        if final_response.is_some() && expected.final_delivery_response_item_id.is_some() {
            return Err(CodexErr::InvalidRequest(
                "final response delivery is already claimed".into(),
            ));
        }
        let mut replacement = expected.clone();
        if let Some(final_response) = final_response {
            replacement.final_response = final_response;
        }
        if let Some(task_preview) = task_preview {
            replacement.task_preview = Some(task_preview);
        }
        if let Some(task) = task {
            replacement.promoted_task_context = Some(
                AgentResponsePromotedTaskContext::from_response_item(task)
                    .ok_or_else(|| CodexErr::InvalidRequest("invalid user task context".into()))?,
            );
        }
        drop(state);
        let mut snapshots = self.response_observation_snapshots(parent, child);
        let snapshot = snapshots
            .iter_mut()
            .find(|snapshot| snapshot.target_turn_id == turn_id)
            .ok_or_else(|| {
                CodexErr::InvalidRequest("missing durable response observation".into())
            })?;
        snapshot.final_delivery = replacement.final_response.into();
        snapshot.task_preview = replacement.task_preview.clone();
        snapshot.promoted_task_context = replacement.promoted_task_context.clone();
        Ok(PreparedUserObservation {
            parent,
            child,
            turn_id,
            expected,
            replacement,
            snapshots,
        })
    }

    pub(in crate::agent::control) fn commit_user_observation(
        &self,
        prepared: PreparedUserObservation,
    ) -> CodexResult<()> {
        let mut state = self.wait_agent_presentations.state();
        let current = state
            .response_observation_by_observer_child
            .get_mut(&(prepared.parent, prepared.child))
            .filter(|relationship| !relationship.revoked)
            .and_then(|relationship| match prepared.turn_id.as_deref() {
                Some(turn) => relationship.turns.get_mut(turn),
                None => relationship.pending_next_turn.as_mut(),
            })
            .ok_or_else(|| {
                CodexErr::Fatal("observation changed during canonical publication".into())
            })?;
        if current != &prepared.expected {
            return Err(CodexErr::Fatal(
                "observation changed during canonical publication".into(),
            ));
        }
        *current = prepared.replacement;
        drop(state);
        self.publish_response_observation_binding();
        Ok(())
    }
}
