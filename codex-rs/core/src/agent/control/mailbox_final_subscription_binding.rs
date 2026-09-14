use super::super::AgentControl;
use super::super::InitialTerminalObservation;
use super::super::ResponseObservationBinding;
use super::super::SessionPresentationId;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_thread_store::MailboxFinalSubscription;
use codex_thread_store::MailboxFinalSubscriptionAuthority;
use codex_thread_store::MailboxFinalSubscriptionState;
use std::collections::HashSet;
use std::sync::Arc;

#[path = "mailbox_final_subscription_recovery.rs"]
mod recovery;

enum MailboxFinalSubscriptionBindOutcome {
    Complete,
    Retry,
}

impl AgentControl {
    pub(crate) async fn read_mailbox_final_subscription_authority(
        &self,
        manager: &crate::thread_manager::ThreadManagerState,
        receiver_thread_id: ThreadId,
        sender_thread_id: ThreadId,
    ) -> CodexResult<MailboxFinalSubscriptionAuthority> {
        if let Some(graph_store) = manager.agent_graph_store()
            && graph_store.supports_agent_aliases()
        {
            let mut thread_ids = vec![receiver_thread_id, sender_thread_id];
            thread_ids.sort_by_key(ToString::to_string);
            thread_ids.dedup();
            let epochs = graph_store
                .read_thread_lifecycle_authority_epochs(thread_ids)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "failed to read mailbox endpoint lifecycle authority: {error}"
                    ))
                })?
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>();
            let receiver_lifecycle_epoch =
                epochs.get(&receiver_thread_id).copied().ok_or_else(|| {
                    CodexErr::Fatal(format!(
                        "missing lifecycle authority for mailbox receiver {receiver_thread_id}"
                    ))
                })?;
            let sender_lifecycle_epoch =
                epochs.get(&sender_thread_id).copied().ok_or_else(|| {
                    CodexErr::Fatal(format!(
                        "missing lifecycle authority for mailbox sender {sender_thread_id}"
                    ))
                })?;
            return Ok(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch,
                sender_lifecycle_epoch,
            });
        }

        let receiver_lifecycle_epoch = i64::try_from(
            manager.agent_lifecycle_generation(receiver_thread_id),
        )
        .map_err(|_| {
            CodexErr::Fatal(format!(
                "mailbox receiver lifecycle generation overflowed for {receiver_thread_id}"
            ))
        })?;
        let sender_lifecycle_epoch =
            i64::try_from(manager.agent_lifecycle_generation(sender_thread_id)).map_err(|_| {
                CodexErr::Fatal(format!(
                    "mailbox sender lifecycle generation overflowed for {sender_thread_id}"
                ))
            })?;
        Ok(MailboxFinalSubscriptionAuthority {
            receiver_lifecycle_epoch,
            sender_lifecycle_epoch,
        })
    }

    /// Installs exact-turn final observers after mailbox acknowledgement commits.
    ///
    /// Callers must release the receiver's messaging and durable-context guards before
    /// entering this method. The receiver lifecycle, destination mailbox, permission, and
    /// sender observation locks are then reacquired in their global order.
    pub(crate) async fn bind_mailbox_final_subscriptions(
        &self,
        receiver: SessionPresentationId,
        subscriptions: Vec<MailboxFinalSubscription>,
    ) {
        for subscription in subscriptions {
            self.bind_mailbox_final_subscription(receiver, subscription)
                .await;
        }
    }

    pub(super) async fn bind_mailbox_final_subscription(
        &self,
        receiver: SessionPresentationId,
        subscription: MailboxFinalSubscription,
    ) {
        if subscription.receiver_thread_id != receiver.thread_id {
            return;
        }
        let Ok(manager) = self.upgrade() else {
            return;
        };
        let Ok(sender_thread) = manager
            .get_thread_including_pending(subscription.sender_thread_id)
            .await
        else {
            return;
        };
        let sender_presentation = sender_thread.session.presentation_id();
        if !sender_thread
            .session
            .services
            .agent_control
            .matches_session_id(self.session_id())
        {
            return;
        }
        let mut retry_delay = std::time::Duration::from_millis(100);
        loop {
            if !self
                .mailbox_final_subscription_endpoints_are_current(
                    &manager,
                    receiver,
                    sender_presentation,
                )
                .await
            {
                return;
            }
            let delivered_final_turns = super::super::resume_delivery::delivered_final_turns(
                &manager,
                sender_presentation.thread_id,
                receiver.thread_id,
            )
            .await;
            match self
                .bind_mailbox_final_subscription_once(
                    &manager,
                    receiver,
                    sender_presentation,
                    &subscription,
                    &delivered_final_turns,
                )
                .await
            {
                MailboxFinalSubscriptionBindOutcome::Complete => return,
                MailboxFinalSubscriptionBindOutcome::Retry => {
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(2));
                }
            }
        }
    }

    async fn mailbox_final_subscription_endpoints_are_current(
        &self,
        manager: &crate::thread_manager::ThreadManagerState,
        receiver: SessionPresentationId,
        sender: SessionPresentationId,
    ) -> bool {
        let Ok(receiver_thread) = manager
            .get_thread_including_pending(receiver.thread_id)
            .await
        else {
            return false;
        };
        let Ok(sender_thread) = manager.get_thread_including_pending(sender.thread_id).await else {
            return false;
        };
        receiver_thread.session.presentation_id() == receiver
            && sender_thread.session.presentation_id() == sender
    }

    async fn bind_mailbox_final_subscription_once(
        &self,
        manager: &Arc<crate::thread_manager::ThreadManagerState>,
        receiver: SessionPresentationId,
        sender: SessionPresentationId,
        requested: &MailboxFinalSubscription,
        delivered_final_turns: &HashSet<String>,
    ) -> MailboxFinalSubscriptionBindOutcome {
        let receiver_lifecycle = manager
            .agent_lifecycle_lock(receiver.thread_id)
            .lock_owned()
            .await;
        let Ok(receiver_thread) = manager
            .get_thread_including_pending(receiver.thread_id)
            .await
        else {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        };
        if receiver_thread.session.presentation_id() != receiver {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        let Ok(sender_thread) = manager.get_thread_including_pending(sender.thread_id).await else {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        };
        if sender_thread.session.presentation_id() != sender
            || !sender_thread
                .session
                .services
                .agent_control
                .matches_session_id(self.session_id())
        {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        let Ok(sender_submission_permit) = self
            .acquire_mailbox_submission_permit(sender.thread_id)
            .await
        else {
            return MailboxFinalSubscriptionBindOutcome::Retry;
        };
        let permission = self.acquire_messaging_permission_transaction().await;
        let observation_transaction = self.acquire_response_observation_transaction(sender).await;
        let store = &sender_thread.session.services.thread_store;
        let active = match store
            .lookup_active_mailbox_final_subscription(receiver.thread_id, sender.thread_id)
            .await
        {
            Ok(Some(active)) => active,
            Ok(None) => return MailboxFinalSubscriptionBindOutcome::Complete,
            Err(error) => {
                tracing::warn!(
                    %error,
                    message_id = %requested.message_id,
                    "failed to recheck active mailbox final subscription; retrying"
                );
                return MailboxFinalSubscriptionBindOutcome::Retry;
            }
        };
        if active.message_id != requested.message_id {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        let authority = match self
            .read_mailbox_final_subscription_authority(
                manager,
                receiver.thread_id,
                sender.thread_id,
            )
            .await
        {
            Ok(authority) => authority,
            Err(error) => {
                tracing::warn!(
                    %error,
                    message_id = %active.message_id,
                    "failed to recheck mailbox final subscription authority; retrying"
                );
                return MailboxFinalSubscriptionBindOutcome::Retry;
            }
        };
        let child = receiver_thread.session.presentation_id();
        if active.receiver_lifecycle_epoch != authority.receiver_lifecycle_epoch
            || active.sender_lifecycle_epoch != authority.sender_lifecycle_epoch
        {
            self.clear_mailbox_final_subscription(sender, child, &active.message_id, None);
            if let Err(error) = store
                .supersede_mailbox_final_subscription_message(
                    receiver.thread_id,
                    active.message_id.clone(),
                )
                .await
            {
                tracing::warn!(
                    %error,
                    message_id = %active.message_id,
                    "failed to retire stale mailbox final subscription; retrying"
                );
                return MailboxFinalSubscriptionBindOutcome::Retry;
            }
            if !self
                .persist_response_observation_snapshot(sender, child)
                .await
            {
                return MailboxFinalSubscriptionBindOutcome::Retry;
            }
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        if self
            .mailbox_final_subscription_message_id(sender, child)
            .is_some_and(|current| current != active.message_id)
        {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        let subscription_was_suppressed =
            self.mailbox_final_subscription_was_suppressed(sender, child, &active.message_id);
        if subscription_was_suppressed
            && let Err(error) = store
                .supersede_mailbox_final_subscription_message(
                    receiver.thread_id,
                    active.message_id.clone(),
                )
                .await
        {
            tracing::warn!(
                %error,
                message_id = %active.message_id,
                "suppressed mailbox final subscription could not be retired; retrying"
            );
            return MailboxFinalSubscriptionBindOutcome::Retry;
        }
        if subscription_was_suppressed {
            self.clear_mailbox_final_subscription(sender, child, &active.message_id, None);
        }
        if subscription_was_suppressed
            && !self
                .persist_response_observation_snapshot(sender, child)
                .await
        {
            return MailboxFinalSubscriptionBindOutcome::Retry;
        }
        if subscription_was_suppressed {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        if !self.install_mailbox_final_subscription(sender, child, &active.message_id) {
            return MailboxFinalSubscriptionBindOutcome::Complete;
        }
        let (policy, initial_terminal_observation, target_turn_id) =
            match (active.state, active.bound_turn_id.as_deref()) {
                (MailboxFinalSubscriptionState::Pending, None) => {
                    if !self
                        .persist_response_observation_snapshot(sender, child)
                        .await
                    {
                        return MailboxFinalSubscriptionBindOutcome::Retry;
                    }
                    return MailboxFinalSubscriptionBindOutcome::Complete;
                }
                (MailboxFinalSubscriptionState::Bound, Some(turn_id)) if !turn_id.is_empty() => {
                    self.bind_mailbox_final_subscription_to_turn(
                        sender,
                        child,
                        &active.message_id,
                        turn_id,
                    );
                    (
                        ResponseObservationPolicy::from_parts(
                            /*commentary*/ false,
                            FinalResponseObservation::Wake,
                        ),
                        InitialTerminalObservation::MailboxFinalTurn(turn_id.to_string()),
                        Some(turn_id.to_string()),
                    )
                }
                _ => return MailboxFinalSubscriptionBindOutcome::Complete,
            };
        let target_lifecycle_generation = manager.agent_lifecycle_generation(receiver.thread_id);
        if let Err(error) = self
            .ensure_v1_response_observer_for_thread(
                manager,
                &receiver_thread,
                sender,
                target_lifecycle_generation,
                policy,
                /*retain_passive_completion_relationship*/ false,
                ResponseObservationBinding::NextTurn,
                initial_terminal_observation,
                target_turn_id,
                /*task_preview*/ None,
                delivered_final_turns,
                /*selection*/ None,
                super::super::response_delivery::ResponseObservationRollbackPolicy::RetryAuthoritativeMailboxProjection,
            )
            .await
        {
            tracing::warn!(
                %error,
                message_id = %active.message_id,
                "bound mailbox final subscription installation failed; retrying"
            );
            return MailboxFinalSubscriptionBindOutcome::Retry;
        }
        drop(observation_transaction);
        drop(permission);
        drop(sender_submission_permit);
        drop(receiver_lifecycle);
        MailboxFinalSubscriptionBindOutcome::Complete
    }
}
