use super::super::AgentControl;
use super::super::SessionPresentationId;
use crate::codex_thread::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::MailboxFinalSubscription;
use codex_thread_store::MailboxFinalSubscriptionAuthority;
use codex_thread_store::MailboxFinalSubscriptionRequest;
use codex_thread_store::MailboxFinalSubscriptionState;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use codex_thread_store::StoredMailboxInput;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::OwnedSemaphorePermit;

#[cfg(test)]
#[path = "mailbox_final_subscription_acceptance_tests.rs"]
mod tests;

fn mailbox_final_subscription_authority_matches(
    subscription: &MailboxFinalSubscription,
    authority: CodexResult<MailboxFinalSubscriptionAuthority>,
) -> CodexResult<bool> {
    authority.map(|authority| {
        subscription.receiver_lifecycle_epoch == authority.receiver_lifecycle_epoch
            && subscription.sender_lifecycle_epoch == authority.sender_lifecycle_epoch
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_mailbox_final_subscription_acceptance(
    control: AgentControl,
    manager: Arc<crate::thread_manager::ThreadManagerState>,
    source: Arc<CodexThread>,
    target_thread: Option<Arc<CodexThread>>,
    receiver: ThreadId,
    sender: SessionPresentationId,
    submission_key: String,
    payload: Option<MailboxPayload>,
    prior_acceptance: Option<StoredMailboxInput>,
    is_fresh_acceptance: bool,
    recipient_lifecycle: OwnedMutexGuard<()>,
    sender_submission_permit: OwnedSemaphorePermit,
    permission: OwnedMutexGuard<()>,
    observation_transaction: OwnedMutexGuard<()>,
) -> CodexResult<StoredMailboxInput> {
    let store = Arc::clone(&source.session.services.thread_store);
    let expected_payload = payload.clone();
    let accepted = if let Some(prior_acceptance) = prior_acceptance {
        prior_acceptance
    } else {
        let payload = payload.ok_or_else(|| {
            CodexErr::Fatal("fresh mailbox acceptance lost its payload".to_string())
        })?;
        let params = AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: submission_key.clone(),
            payload,
            final_subscription: MailboxFinalSubscriptionRequest::Wake,
        };
        let authority = control
            .read_mailbox_final_subscription_authority(&manager, receiver, sender.thread_id)
            .await?;
        match store
            .accept_mailbox_input_with_authority(params, authority)
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => {
                let acceptance_error = error.to_string();
                loop {
                    match store.lookup_mailbox_input(receiver, &submission_key).await {
                        Ok(Some(accepted)) => break accepted,
                        Ok(None) => {
                            return Err(CodexErr::Fatal(format!(
                                "failed to accept mailbox input: {acceptance_error}"
                            )));
                        }
                        Err(lookup_error) => {
                            tracing::warn!(
                                %lookup_error,
                                %receiver,
                                "mailbox acceptance outcome is ambiguous; retrying durable lookup"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
    };
    if accepted.receiver_thread_id != receiver
        || accepted.submission_key != submission_key
        || accepted.sender != MailboxSender::Agent(sender.thread_id)
        || expected_payload
            .as_ref()
            .is_some_and(|payload| &accepted.payload != payload)
        || accepted.final_subscription.is_none()
    {
        return Err(CodexErr::InvalidRequest(
            "mailbox invocation already accepted different input, authorship, or subscription intent"
                .to_string(),
        ));
    }

    // Fresh acceptance committed under the same messaging and observer locks. Its returned row
    // is authoritative here; the detached binder rechecks SQL on every retry after releasing
    // these guards.
    let active_subscription = accepted
        .final_subscription
        .as_ref()
        .filter(|subscription| {
            matches!(
                subscription.state,
                MailboxFinalSubscriptionState::Pending | MailboxFinalSubscriptionState::Bound
            )
        })
        .cloned();

    let binding_receiver = target_thread
        .as_ref()
        .map(|thread| thread.session.presentation_id());
    if let Some(subscription) = active_subscription.as_ref() {
        let child = control.mailbox_final_subscription_child(sender, receiver, binding_receiver);
        let authority_matches = match mailbox_final_subscription_authority_matches(
            subscription,
            control
                .read_mailbox_final_subscription_authority(&manager, receiver, sender.thread_id)
                .await,
        ) {
            Ok(matches) => Some(matches),
            Err(error) => {
                tracing::warn!(
                    %error,
                    message_id = %subscription.message_id,
                    "mailbox subscription authority could not be rechecked after acceptance; preserving the accepted token for durable retry"
                );
                None
            }
        };
        if authority_matches == Some(false) {
            control.clear_mailbox_final_subscription(sender, child, &subscription.message_id, None);
            if let Err(error) = store
                .supersede_mailbox_final_subscription_message(
                    receiver,
                    subscription.message_id.clone(),
                )
                .await
            {
                tracing::warn!(
                    %error,
                    message_id = %subscription.message_id,
                    "stale mailbox final subscription could not be retired yet"
                );
            }
        } else if control.install_mailbox_final_subscription(
            sender,
            child,
            &subscription.message_id,
        ) {
            match (&subscription.state, &subscription.bound_turn_id) {
                (MailboxFinalSubscriptionState::Pending, None) => {}
                (MailboxFinalSubscriptionState::Bound, Some(turn_id))
                    if !turn_id.is_empty() && authority_matches == Some(true) =>
                {
                    control.bind_mailbox_final_subscription_to_turn(
                        sender,
                        child,
                        &subscription.message_id,
                        turn_id,
                    );
                }
                (MailboxFinalSubscriptionState::Bound, Some(turn_id)) if !turn_id.is_empty() => {}
                _ => {
                    tracing::warn!(
                        message_id = %subscription.message_id,
                        "mailbox final subscription has an invalid active state"
                    );
                    control.clear_mailbox_final_subscription(
                        sender,
                        child,
                        &subscription.message_id,
                        None,
                    );
                    if let Err(error) = store
                        .supersede_mailbox_final_subscription_message(
                            receiver,
                            subscription.message_id.clone(),
                        )
                        .await
                    {
                        tracing::warn!(
                            %error,
                            message_id = %subscription.message_id,
                            "invalid mailbox final subscription could not be retired"
                        );
                    }
                }
            }
        } else {
            control.clear_mailbox_final_subscription(sender, child, &subscription.message_id, None);
            if let Err(error) = store
                .supersede_mailbox_final_subscription_message(
                    receiver,
                    subscription.message_id.clone(),
                )
                .await
            {
                tracing::warn!(
                    %error,
                    message_id = %subscription.message_id,
                    "suppressed mailbox final subscription could not be retired"
                );
            }
        }
        if !control
            .persist_response_observation_snapshot(sender, child)
            .await
        {
            tracing::warn!(
                %receiver,
                sender_thread_id = %sender.thread_id,
                "accepted mailbox final subscription audit could not be persisted; retrying durable projection"
            );
        }
    }

    drop(observation_transaction);
    drop(permission);
    drop(sender_submission_permit);
    drop(recipient_lifecycle);

    if let (Some(receiver_presentation), Some(subscription)) =
        (binding_receiver, active_subscription)
    {
        let control = control.clone();
        tokio::spawn(async move {
            control
                .bind_mailbox_final_subscription(receiver_presentation, subscription)
                .await;
        });
    }

    if is_fresh_acceptance
        && let MailboxPayload::Agent { attribution, input } = &accepted.payload
        && let Some(id) = codex_protocol::mailbox_acceptance_receipt_id(&accepted.id)
    {
        let mut receipt = codex_protocol::items::AgentMessageItem::new(&[]);
        receipt.id = id.to_string();
        receipt.phase = Some(codex_protocol::models::MessagePhase::Commentary);
        receipt.attribution = Some(attribution.as_ref().clone());
        receipt.input = Some(input.clone());
        if let Err(error) = control
            .mirror_attributed_agent_input(
                receiver,
                &codex_protocol::items::TurnItem::AgentMessage(receipt),
            )
            .await
        {
            tracing::warn!(%error, "failed to present mailbox acceptance notice");
        }
    }
    control.notify_mailbox_activity(receiver).await;
    Ok(accepted)
}
