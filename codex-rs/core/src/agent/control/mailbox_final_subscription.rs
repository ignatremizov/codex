use super::AgentControl;
use super::SessionPresentationId;
use crate::codex_thread::CodexThread;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_thread_store::MailboxFinalSubscription;
use codex_thread_store::MailboxFinalSubscriptionState;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::OwnedSemaphorePermit;

#[path = "mailbox_final_subscription_acceptance.rs"]
mod acceptance;
#[path = "mailbox_final_subscription_binding.rs"]
mod binding;

pub(super) use acceptance::complete_mailbox_final_subscription_acceptance;

pub(super) struct MailboxFinalSubscriptionReconciliation {
    pub(super) observations: Vec<codex_protocol::protocol::AgentResponseObservation>,
    _sender_submission_permit: OwnedSemaphorePermit,
    _messaging_permission: OwnedMutexGuard<()>,
    _observation_transaction: OwnedMutexGuard<()>,
}

#[cfg(test)]
#[path = "mailbox_final_subscription_tests.rs"]
mod tests;

impl AgentControl {
    pub(super) fn overlay_mailbox_final_subscription_observations(
        &self,
        observer: SessionPresentationId,
        target: SessionPresentationId,
        mut observations: Vec<codex_protocol::protocol::AgentResponseObservation>,
        active: Option<&MailboxFinalSubscription>,
        stale_message_ids: &std::collections::HashSet<String>,
        delivered_final_turns: &HashSet<String>,
    ) -> Vec<codex_protocol::protocol::AgentResponseObservation> {
        if (active.is_some() || !stale_message_ids.is_empty())
            && !observations
                .iter()
                .any(|observation| observation.target_turn_id.is_none())
            && let Some(root_observation) = self
                .response_observation_audit_snapshots(observer, target, None)
                .into_iter()
                .find(|observation| observation.target_turn_id.is_none())
        {
            observations.push(root_observation);
        }
        if let Some(MailboxFinalSubscription {
            state: MailboxFinalSubscriptionState::Bound,
            bound_turn_id: Some(turn_id),
            ..
        }) = active
            && !observations
                .iter()
                .any(|observation| observation.target_turn_id.as_deref() == Some(turn_id.as_str()))
            && let Some(turn_observation) = self
                .response_observation_audit_snapshots(observer, target, Some(turn_id.clone()))
                .into_iter()
                .find(|observation| observation.target_turn_id.as_deref() == Some(turn_id))
        {
            observations.push(turn_observation);
        }

        for observation in &mut observations {
            if observation.observer_thread_id != observer.thread_id
                || observation.target_thread_id != target.thread_id
            {
                continue;
            }
            if observation.target_turn_id.is_none()
                && let Some(active) = active
            {
                observation.baseline_final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                observation.final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                observation.mailbox_final_subscription_message_id = Some(active.message_id.clone());
                observation.mailbox_final_subscription_suppressed_message_id = None;
                continue;
            }
            if observation.target_turn_id.is_none()
                && let Some(message_id) = observation
                    .mailbox_final_subscription_message_id
                    .as_ref()
                    .filter(|message_id| stale_message_ids.contains(*message_id))
                    .cloned()
            {
                observation.mailbox_final_subscription_message_id = None;
                observation.mailbox_final_subscription_suppressed_message_id = Some(message_id);
                observation.baseline_final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                observation.final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                continue;
            }
            if observation.target_turn_id.is_none() {
                continue;
            }

            let Some(turn_id) = observation.target_turn_id.as_deref() else {
                continue;
            };
            let has_admitted_final = delivered_final_turns.contains(turn_id)
                || observation.final_delivery_response_item_id.is_some();
            if active.is_some()
                && !has_admitted_final
                && observation.final_delivery
                    != codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly
            {
                observation.final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                observation.mailbox_final_subscription_message_id = None;
            }
            if let Some(active) = active
                && active.bound_turn_id.as_deref() == Some(turn_id)
                && !has_admitted_final
            {
                observation.final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::Wake;
                observation.mailbox_final_subscription_message_id = Some(active.message_id.clone());
            }
            if active.is_none()
                && observation
                    .mailbox_final_subscription_message_id
                    .as_ref()
                    .is_some_and(|message_id| stale_message_ids.contains(message_id))
                && !has_admitted_final
                && observation.final_delivery
                    != codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly
            {
                observation.final_delivery =
                    codex_protocol::protocol::AgentResponseFinalDelivery::None;
                observation.mailbox_final_subscription_message_id = None;
            }
        }
        observations
    }

    pub(super) async fn reconcile_mailbox_final_subscription_observations(
        &self,
        manager: &Arc<crate::thread_manager::ThreadManagerState>,
        observer: SessionPresentationId,
        target: SessionPresentationId,
        observer_thread: &CodexThread,
        observations: Vec<codex_protocol::protocol::AgentResponseObservation>,
    ) -> CodexResult<MailboxFinalSubscriptionReconciliation> {
        let delivered_final_turns = super::resume_delivery::delivered_final_turns(
            manager,
            observer.thread_id,
            target.thread_id,
        )
        .await;
        let sender_submission_permit = self
            .acquire_mailbox_submission_permit(observer.thread_id)
            .await?;
        let messaging_permission = self.acquire_messaging_permission_transaction().await;
        let observation_transaction = self
            .acquire_response_observation_transaction(observer)
            .await;
        let store = &observer_thread.session.services.thread_store;
        let active = store
            .lookup_active_mailbox_final_subscription(target.thread_id, observer.thread_id)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to read active mailbox final subscription for {}: {error}",
                    target.thread_id
                ))
            })?;
        let mut token_ids = observations
            .iter()
            .flat_map(|observation| {
                [
                    observation.mailbox_final_subscription_message_id.as_deref(),
                    observation
                        .mailbox_final_subscription_suppressed_message_id
                        .as_deref(),
                ]
                .into_iter()
                .flatten()
                .map(ToOwned::to_owned)
            })
            .collect::<std::collections::HashSet<_>>();
        if let Some(active) = active.as_ref() {
            token_ids.insert(active.message_id.clone());
        }
        let mut subscriptions_by_message = std::collections::HashMap::new();
        for message_id in token_ids {
            let subscription = store
                .lookup_mailbox_final_subscription(target.thread_id, message_id.clone())
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "failed to read mailbox final subscription {message_id}: {error}"
                    ))
                })?;
            subscriptions_by_message.insert(message_id, subscription);
        }

        let mut stale_message_ids = std::collections::HashSet::new();
        let current_authority = if active.is_some()
            || subscriptions_by_message
                .values()
                .flatten()
                .any(|subscription| {
                    matches!(
                        subscription.state,
                        MailboxFinalSubscriptionState::Pending
                            | MailboxFinalSubscriptionState::Bound
                    )
                }) {
            Some(
                self.read_mailbox_final_subscription_authority(
                    manager,
                    target.thread_id,
                    observer.thread_id,
                )
                .await?,
            )
        } else {
            None
        };
        let active = active.filter(|subscription| {
            subscriptions_by_message
                .get(&subscription.message_id)
                .and_then(Option::as_ref)
                .is_some_and(|stored| {
                    stored.state == subscription.state
                        && stored.bound_turn_id == subscription.bound_turn_id
                        && current_authority.is_some_and(|authority| {
                            stored.receiver_lifecycle_epoch == authority.receiver_lifecycle_epoch
                                && stored.sender_lifecycle_epoch == authority.sender_lifecycle_epoch
                        })
                })
        });
        for (message_id, subscription) in &subscriptions_by_message {
            let is_current_active = active
                .as_ref()
                .is_some_and(|active| active.message_id == *message_id);
            let Some(subscription) = subscription.as_ref() else {
                stale_message_ids.insert(message_id.clone());
                continue;
            };
            let lifecycle_matches = current_authority.is_some_and(|authority| {
                subscription.receiver_lifecycle_epoch == authority.receiver_lifecycle_epoch
                    && subscription.sender_lifecycle_epoch == authority.sender_lifecycle_epoch
            });
            if matches!(
                subscription.state,
                MailboxFinalSubscriptionState::Pending | MailboxFinalSubscriptionState::Bound
            ) && !lifecycle_matches
            {
                store
                    .supersede_mailbox_final_subscription_message(
                        target.thread_id,
                        message_id.clone(),
                    )
                    .await
                    .map_err(|error| {
                        CodexErr::Fatal(format!(
                            "failed to retire stale mailbox final subscription {message_id}: {error}"
                        ))
                    })?;
            }
            if !is_current_active || !lifecycle_matches {
                stale_message_ids.insert(message_id.clone());
            }
        }
        let effective_observations = self.overlay_mailbox_final_subscription_observations(
            observer,
            target,
            observations.clone(),
            active.as_ref(),
            &stale_message_ids,
            &delivered_final_turns,
        );

        if let Some(active) = active.as_ref()
            && !self.install_mailbox_final_subscription(observer, target, &active.message_id)
        {
            store
                .supersede_mailbox_final_subscription_message(
                    target.thread_id,
                    active.message_id.clone(),
                )
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "failed to retire suppressed mailbox final subscription {}: {error}",
                        active.message_id
                    ))
                })?;
            stale_message_ids.insert(active.message_id.clone());
            let effective_observations = self.overlay_mailbox_final_subscription_observations(
                observer,
                target,
                observations.clone(),
                None,
                &stale_message_ids,
                &delivered_final_turns,
            );
            if effective_observations != observations
                && !self
                    .persist_response_observation_updates(observer, effective_observations.clone())
                    .await
            {
                return Err(CodexErr::Fatal(
                    "failed to persist suppressed mailbox final subscription".to_string(),
                ));
            }
            return Ok(MailboxFinalSubscriptionReconciliation {
                observations: effective_observations,
                _sender_submission_permit: sender_submission_permit,
                _messaging_permission: messaging_permission,
                _observation_transaction: observation_transaction,
            });
        }
        if let Some(active) = active.as_ref()
            && let (MailboxFinalSubscriptionState::Bound, Some(turn_id)) =
                (active.state, active.bound_turn_id.as_deref())
        {
            self.bind_mailbox_final_subscription_to_turn(
                observer,
                target,
                &active.message_id,
                turn_id,
            );
        }
        if active.is_none() {
            for message_id in &stale_message_ids {
                self.clear_mailbox_final_subscription(observer, target, message_id, None);
            }
        }
        if effective_observations != observations
            && !self
                .persist_response_observation_updates(observer, effective_observations.clone())
                .await
        {
            return Err(CodexErr::Fatal(
                "failed to persist mailbox final subscription reconciliation".to_string(),
            ));
        }
        Ok(MailboxFinalSubscriptionReconciliation {
            observations: effective_observations,
            _sender_submission_permit: sender_submission_permit,
            _messaging_permission: messaging_permission,
            _observation_transaction: observation_transaction,
        })
    }
}
