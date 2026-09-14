use super::super::*;
use crate::CodexThread;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::MailboxFinalSubscription;
use codex_thread_store::MailboxFinalSubscriptionAuthority;
use codex_thread_store::MailboxFinalSubscriptionState;
use codex_thread_store::StoredMailboxInput;

pub(super) fn mailbox_final_subscription_authority_matches(
    subscription: &MailboxFinalSubscription,
    authority: CodexResult<MailboxFinalSubscriptionAuthority>,
) -> CodexResult<bool> {
    authority.map(|authority| {
        subscription.receiver_lifecycle_epoch == authority.receiver_lifecycle_epoch
            && subscription.sender_lifecycle_epoch == authority.sender_lifecycle_epoch
    })
}

impl LocalAgentControl {
    /// Called inside the retained acceptance worker with lifecycle, destination, permission
    /// and observer guards. A lost write acknowledgement permits lookup, never another write.
    pub(in crate::agent::control) async fn accept_mailbox_subscription(
        &self,
        source: &Arc<CodexThread>,
        params: AcceptMailboxInputParams,
    ) -> CodexResult<StoredMailboxInput> {
        let manager = self.upgrade()?;
        let authority = self
            .read_mailbox_final_subscription_authority(
                &manager,
                params.receiver_thread_id,
                source.session.thread_id(),
            )
            .await?;
        let store = &source.session.services.thread_store;
        let receiver = params.receiver_thread_id;
        let key = params.submission_key.clone();
        let expected_payload = params.payload.clone();
        let accepted = match store
            .accept_mailbox_input_with_authority(params, authority)
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => loop {
                match store.lookup_mailbox_input(receiver, &key).await {
                    Ok(Some(accepted)) => break accepted,
                    Ok(None) => {
                        return Err(CodexErr::Fatal(format!(
                            "mailbox acceptance failed: {error}"
                        )));
                    }
                    Err(lookup_error) => {
                        tracing::warn!(%lookup_error, %receiver, "mailbox acceptance outcome unknown; retrying lookup");
                        tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 100)).await;
                    }
                }
            },
        };
        if accepted.receiver_thread_id != receiver
            || accepted.submission_key != key
            || accepted.sender
                != codex_thread_store::MailboxSender::Agent(source.session.thread_id())
            || accepted.payload != expected_payload
            || accepted.final_subscription.is_none()
        {
            return Err(CodexErr::InvalidRequest(
                "mailbox acceptance key has different immutable intent".into(),
            ));
        }
        Ok(accepted)
    }

    /// Suppress old unclaimed final policies before returning a committed zf acceptance.
    /// Re-read active identity even for retries: an older accepted row is immutable evidence,
    /// not authority to replace a newer token or a later explicit response policy.
    pub(in crate::agent::control) async fn project_accepted_mailbox_subscription(
        &self,
        source: &Arc<CodexThread>,
        accepted: &StoredMailboxInput,
    ) -> CodexResult<()> {
        let Some(intent) = accepted.final_subscription.as_ref() else {
            return Ok(());
        };
        let manager = self.upgrade()?;
        let store = &source.session.services.thread_store;
        let Some(active) = store
            .lookup_active_mailbox_final_subscription(
                accepted.receiver_thread_id,
                source.session.thread_id(),
            )
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
        else {
            return Ok(());
        };
        if active.message_id != intent.message_id {
            return Ok(());
        }
        let authority = self
            .read_mailbox_final_subscription_authority(
                &manager,
                active.receiver_thread_id,
                active.sender_thread_id,
            )
            .await;
        let authority_matches = match mailbox_final_subscription_authority_matches(
            &active, authority,
        ) {
            Ok(matches) => Some(matches),
            Err(error) => {
                tracing::warn!(%error, "accepted mailbox token awaits authoritative epoch recheck");
                None
            }
        };
        if authority_matches == Some(false) {
            store
                .supersede_mailbox_final_subscription_message(
                    active.receiver_thread_id,
                    active.message_id,
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            return Ok(());
        }
        let Ok(target) = manager.get_thread(active.receiver_thread_id).await else {
            return Ok(());
        };
        if !Arc::ptr_eq(&self.state, &target.session.services.agent_control.state) {
            return Ok(());
        }
        let parent = source.session.presentation_id();
        let child = target.session.presentation_id();
        if self.install_mailbox_final_subscription(parent, child, &active.message_id) {
            if authority_matches == Some(true)
                && active.state == MailboxFinalSubscriptionState::Bound
                && let Some(turn_id) = active
                    .bound_turn_id
                    .as_deref()
                    .filter(|turn| !turn.is_empty())
            {
                self.bind_mailbox_final_subscription_to_turn(
                    parent,
                    child,
                    &active.message_id,
                    turn_id,
                );
            }
            self.persist_response_observation_snapshot(parent, child)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "mailbox_final_subscription_acceptance_tests.rs"]
mod tests;
