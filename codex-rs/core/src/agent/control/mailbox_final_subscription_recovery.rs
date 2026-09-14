use super::super::*;

impl LocalAgentControl {
    /// SQL intent is the only cold-recovery authority; audit snapshots remain inert.
    pub(crate) fn schedule_mailbox_final_subscription_recovery(
        &self,
        loaded: SessionPresentationId,
    ) {
        let control = self.clone();
        tokio::spawn(async move {
            let Ok(manager) = control.upgrade() else {
                return;
            };
            let mut delay = std::time::Duration::from_millis(/*millis*/ 100);
            loop {
                let Ok(thread) = manager.get_thread(loaded.thread_id).await else {
                    return;
                };
                if thread.session.presentation_id() != loaded
                    || !Arc::ptr_eq(&control.state, &thread.session.services.agent_control.state)
                    || thread.session.submission_admission.check_ready().is_err()
                {
                    return;
                }
                match thread
                    .session
                    .services
                    .thread_store
                    .read_active_mailbox_final_subscriptions_for_thread(loaded.thread_id)
                    .await
                {
                    Ok(subscriptions) => {
                        for subscription in subscriptions {
                            if let Ok(receiver) =
                                manager.get_thread(subscription.receiver_thread_id).await
                            {
                                control
                                    .bind_mailbox_final_subscriptions(
                                        receiver.session.presentation_id(),
                                        vec![subscription],
                                    )
                                    .await;
                            }
                        }
                        return;
                    }
                    Err(codex_thread_store::ThreadStoreError::Unsupported { .. }) => return,
                    Err(error) => {
                        tracing::warn!(%error, "mailbox subscription recovery lookup failed")
                    }
                }
                tokio::time::sleep(delay).await;
                delay = delay
                    .saturating_mul(/*rhs*/ 2)
                    .min(std::time::Duration::from_secs(/*secs*/ 2));
            }
        });
    }
}
