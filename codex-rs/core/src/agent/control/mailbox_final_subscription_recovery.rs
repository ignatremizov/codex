use super::super::super::AgentControl;
use super::super::super::SessionPresentationId;
use std::sync::Arc;

impl AgentControl {
    pub(crate) fn schedule_mailbox_final_subscription_recovery(
        &self,
        current: SessionPresentationId,
    ) {
        let control = self.clone();
        tokio::spawn(async move {
            control
                .recover_mailbox_final_subscriptions_for_thread(current)
                .await;
        });
    }

    async fn recover_mailbox_final_subscriptions_for_thread(&self, current: SessionPresentationId) {
        let Ok(manager) = self.upgrade() else {
            return;
        };
        let Ok(current_thread) = manager
            .get_thread_including_pending(current.thread_id)
            .await
        else {
            return;
        };
        if current_thread.session.presentation_id() != current {
            return;
        }
        let store = Arc::clone(&current_thread.session.services.thread_store);
        drop(current_thread);
        let mut retry_delay = std::time::Duration::from_millis(100);
        let subscriptions = loop {
            let Ok(thread) = manager
                .get_thread_including_pending(current.thread_id)
                .await
            else {
                return;
            };
            if thread.session.presentation_id() != current {
                return;
            }
            match store
                .read_active_mailbox_final_subscriptions_for_thread(current.thread_id)
                .await
            {
                Ok(subscriptions) => break subscriptions,
                Err(error)
                    if matches!(
                        error,
                        codex_thread_store::ThreadStoreError::Unsupported { .. }
                    ) =>
                {
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        thread_id = %current.thread_id,
                        "failed to discover active mailbox final subscriptions; retrying"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(2));
                }
            }
        };
        for subscription in subscriptions {
            let permission = self.acquire_messaging_permission_transaction().await;
            let current_subscription = match store
                .lookup_active_mailbox_final_subscription(
                    subscription.receiver_thread_id,
                    subscription.sender_thread_id,
                )
                .await
            {
                Ok(Some(current)) if current.message_id == subscription.message_id => Some(current),
                Ok(_) => {
                    drop(permission);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        message_id = %subscription.message_id,
                        "failed to recheck restored mailbox final subscription"
                    );
                    None
                }
            };
            if let Some(current_subscription) = current_subscription {
                match self
                    .read_mailbox_final_subscription_authority(
                        &manager,
                        current_subscription.receiver_thread_id,
                        current_subscription.sender_thread_id,
                    )
                    .await
                {
                    Ok(authority)
                        if current_subscription.receiver_lifecycle_epoch
                            != authority.receiver_lifecycle_epoch
                            || current_subscription.sender_lifecycle_epoch
                                != authority.sender_lifecycle_epoch =>
                    {
                        if let Err(error) = store
                            .supersede_mailbox_final_subscription_message(
                                current_subscription.receiver_thread_id,
                                current_subscription.message_id.clone(),
                            )
                            .await
                        {
                            tracing::warn!(
                                %error,
                                message_id = %current_subscription.message_id,
                                "failed to retire stale mailbox final subscription"
                            );
                        }
                        drop(permission);
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        %error,
                        message_id = %current_subscription.message_id,
                        "failed to validate restored mailbox final subscription authority"
                    ),
                }
            }
            drop(permission);
            let Ok(receiver_thread) = manager
                .get_thread_including_pending(subscription.receiver_thread_id)
                .await
            else {
                continue;
            };
            let receiver = receiver_thread.session.presentation_id();
            drop(receiver_thread);
            if let Ok(sender_thread) = manager
                .get_thread_including_pending(subscription.sender_thread_id)
                .await
            {
                if !sender_thread
                    .session
                    .services
                    .agent_control
                    .matches_session_id(self.session_id())
                {
                    continue;
                }
                drop(sender_thread);
                self.bind_mailbox_final_subscription(receiver, subscription)
                    .await;
            }
        }
    }
}
