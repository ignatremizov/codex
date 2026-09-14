use super::super::*;
use crate::codex_thread::CodexThread;
use codex_thread_store::MailboxFinalSubscription;
use codex_thread_store::MailboxFinalSubscriptionAuthority;
use codex_thread_store::MailboxFinalSubscriptionState;

impl LocalAgentControl {
    pub(crate) async fn read_mailbox_final_subscription_authority(
        &self,
        manager: &ThreadManagerState,
        receiver: ThreadId,
        sender: ThreadId,
    ) -> CodexResult<MailboxFinalSubscriptionAuthority> {
        let graph = manager
            .agent_graph_store()
            .filter(|graph| graph.supports_agent_aliases())
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation(
                    "mailbox subscriptions require durable lifecycle authority".into(),
                )
            })?;
        let epochs = graph
            .read_thread_lifecycle_authority_epochs(vec![receiver, sender])
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to read mailbox lifecycle authority: {error}",
                ))
            })?
            .into_iter()
            .collect::<HashMap<_, _>>();
        let receiver_lifecycle_epoch = epochs
            .get(&receiver)
            .copied()
            .ok_or_else(|| CodexErr::Fatal("missing mailbox receiver authority".into()))?;
        let sender_lifecycle_epoch = epochs
            .get(&sender)
            .copied()
            .ok_or_else(|| CodexErr::Fatal("missing mailbox sender authority".into()))?;
        Ok(MailboxFinalSubscriptionAuthority {
            receiver_lifecycle_epoch,
            sender_lifecycle_epoch,
        })
    }

    /// Binding is owned independently of the consuming turn. It never loads either endpoint.
    pub(crate) async fn bind_mailbox_final_subscriptions(
        &self,
        receiver: SessionPresentationId,
        subscriptions: Vec<MailboxFinalSubscription>,
    ) {
        if subscriptions.is_empty() {
            return;
        }
        let control = self.clone();
        tokio::spawn(async move {
            for subscription in subscriptions {
                control
                    .bind_mailbox_final_subscription(receiver, subscription)
                    .await;
            }
        });
    }

    async fn bind_mailbox_final_subscription(
        &self,
        receiver: SessionPresentationId,
        subscription: MailboxFinalSubscription,
    ) {
        if subscription.receiver_thread_id != receiver.thread_id {
            return;
        }
        let Ok(manager) = self.upgrade() else { return };
        let Ok(sender) = manager.get_thread(subscription.sender_thread_id).await else {
            return;
        };
        let sender_id = sender.session.presentation_id();
        let mut delay = std::time::Duration::from_millis(/*millis*/ 100);
        loop {
            let Ok(target) = manager.get_thread(receiver.thread_id).await else {
                return;
            };
            let Ok(current_sender) = manager.get_thread(sender_id.thread_id).await else {
                return;
            };
            if target.session.presentation_id() != receiver
                || current_sender.session.presentation_id() != sender_id
                || !Arc::ptr_eq(&self.state, &sender.session.services.agent_control.state)
                || !Arc::ptr_eq(&self.state, &target.session.services.agent_control.state)
                || sender.session.submission_admission.check_ready().is_err()
                || target.session.submission_admission.check_ready().is_err()
            {
                return;
            }
            match self
                .bind_mailbox_final_subscription_once(&manager, &sender, &target, &subscription)
                .await
            {
                Ok(()) => return,
                Err(error) => tracing::warn!(
                    %error,
                    message_id = %subscription.message_id,
                    "mailbox subscription projection failed; rechecking durable token",
                ),
            }
            tokio::time::sleep(delay).await;
            delay = delay
                .saturating_mul(/*rhs*/ 2)
                .min(std::time::Duration::from_secs(/*secs*/ 2));
        }
    }

    async fn bind_mailbox_final_subscription_once(
        &self,
        manager: &ThreadManagerState,
        sender: &Arc<CodexThread>,
        target: &Arc<CodexThread>,
        requested: &MailboxFinalSubscription,
    ) -> CodexResult<()> {
        let parent = sender.session.presentation_id();
        let child = target.session.presentation_id();
        // Proof reads acquire the canonical barrier and must precede observer transactions.
        let history = sender
            .session
            .services
            .thread_store
            .load_mailbox_canonical_history(parent.thread_id)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("mailbox recovery proof unavailable: {error}"))
            })?;
        let delivered = super::super::resume_delivery::delivered_final_turns_in_history(
            &history,
            parent.thread_id,
            child.thread_id,
        );
        // Typed canonical revocations can retire SQL intent, but cannot grant an observer.
        let canonically_retired = history.iter().any(|item| {
            matches!(
                item,
                RolloutItem::AgentResponseObservation(observation)
                    if observation.observer_thread_id == parent.thread_id
                        && observation.target_thread_id == child.thread_id
                        && observation.target_turn_id.is_none()
                        && observation.mailbox_final_subscription_suppressed_message_id.as_deref()
                            == Some(requested.message_id.as_str())
            )
        });
        let _lifecycle = manager
            .acquire_live_agent_lifecycle(child.thread_id)
            .await?;
        let mailbox = self.state.mailbox_submission(parent.thread_id);
        let _destination = Arc::clone(&mailbox.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("mailbox sender closed: {error}")))?;
        let _permission = self.acquire_messaging_permission_transaction().await;
        let _observer = self.acquire_response_observation_transaction(parent).await;
        if manager
            .get_thread(child.thread_id)
            .await?
            .session
            .presentation_id()
            != child
            || manager
                .get_thread(parent.thread_id)
                .await?
                .session
                .presentation_id()
                != parent
        {
            return Ok(());
        }
        sender.session.submission_admission.check_ready()?;
        target.session.submission_admission.check_ready()?;
        let store = &sender.session.services.thread_store;
        let Some(active) = store
            .lookup_active_mailbox_final_subscription(child.thread_id, parent.thread_id)
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
        else {
            return Ok(());
        };
        if active.message_id != requested.message_id {
            return Ok(());
        }
        let authority = self
            .read_mailbox_final_subscription_authority(manager, child.thread_id, parent.thread_id)
            .await?;
        if canonically_retired
            || !super::acceptance::mailbox_final_subscription_authority_matches(
                &active,
                Ok(authority),
            )?
            || self.mailbox_final_subscription_was_suppressed(parent, child, &active.message_id)
        {
            store
                .supersede_mailbox_final_subscription_message(
                    child.thread_id,
                    active.message_id.clone(),
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            self.clear_mailbox_final_subscription(
                parent,
                child,
                &active.message_id,
                /*preserved_turn_id*/ None,
            );
            return self
                .persist_response_observation_snapshot(parent, child)
                .await;
        }
        let turn_id = match (&active.state, &active.bound_turn_id) {
            (MailboxFinalSubscriptionState::Pending, None) => None,
            (MailboxFinalSubscriptionState::Bound, Some(turn)) if !turn.is_empty() => {
                Some(turn.clone())
            }
            (MailboxFinalSubscriptionState::Pending | MailboxFinalSubscriptionState::Bound, _)
            | (
                MailboxFinalSubscriptionState::Delivered
                | MailboxFinalSubscriptionState::Superseded
                | MailboxFinalSubscriptionState::Rejected,
                _,
            ) => {
                return Err(CodexErr::Fatal(
                    "active mailbox subscription has invalid binding".into(),
                ));
            }
        };
        if let Some(turn) = turn_id.as_ref()
            && (delivered.contains(turn)
                || super::delivery::wait_delivered_subscription(&history, &active))
        {
            store
                .acknowledge_mailbox_final_subscription_delivery(
                    child.thread_id,
                    active.message_id.clone(),
                    turn.clone(),
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            self.clear_mailbox_final_subscription(
                parent,
                child,
                &active.message_id,
                /*preserved_turn_id*/ None,
            );
            return Ok(());
        }
        if !self.install_mailbox_final_subscription(parent, child, &active.message_id) {
            return Ok(());
        }
        let Some(turn_id) = turn_id else {
            return self
                .persist_response_observation_snapshot(parent, child)
                .await;
        };
        self.bind_mailbox_final_subscription_to_turn(parent, child, &active.message_id, &turn_id);
        self.install_response_observer(
            sender,
            target,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            ResponseObservationBinding::NextTurn,
            super::super::response_observer::ResponseObserverStart::MailboxFinalTurn { turn_id },
        )
        .await
    }
}
