use super::LocalThreadStore;
use crate::AcceptMailboxInputParams;
use crate::ClaimMailboxInputParams;
use crate::MailboxClaim;
use crate::ReconcileMailboxDeliveryParams;
use crate::StoredMailboxInput;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::mailbox::decode_claim;
use crate::mailbox::decode_message;
use crate::mailbox::encode_payload;
use crate::mailbox::storage_error;

pub(super) async fn lookup(
    store: &LocalThreadStore,
    receiver_thread_id: codex_protocol::ThreadId,
    submission_key: &str,
) -> ThreadStoreResult<Option<StoredMailboxInput>> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "lookup_mailbox_input",
        })?;
    state_db
        .thread_queue()
        .read_mail_by_submission_key(receiver_thread_id, submission_key)
        .await
        .map_err(|error| storage_error(error.as_ref()))?
        .map(decode_message)
        .transpose()
}

pub(super) async fn lookup_claim(
    store: &LocalThreadStore,
    invocation: crate::MailboxInvocation,
) -> ThreadStoreResult<Option<MailboxClaim>> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "lookup_mailbox_claim",
        })?;
    state_db
        .thread_queue()
        .lookup_mail_claim(&invocation)
        .await
        .map_err(|error| storage_error(error.as_ref()))?
        .map(decode_claim)
        .transpose()
}

pub(super) async fn recover(
    store: &LocalThreadStore,
    params: ClaimMailboxInputParams,
) -> ThreadStoreResult<crate::RecoveredMailboxClaim> {
    let current = claim(store, params).await?;
    let history = if current
        .messages
        .iter()
        .any(|member| member.message.state == crate::MailboxMessageState::Claimed)
    {
        load_history(store, current.invocation.receiver_thread_id).await?
    } else {
        Vec::new()
    };
    crate::mailbox_artifacts::recover_claims(&[current], &history)?
        .pop()
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "mailbox recovery did not return its claim".to_string(),
        })
}

pub(super) async fn reject(
    store: &LocalThreadStore,
    params: crate::RejectMailboxInputParams,
) -> ThreadStoreResult<StoredMailboxInput> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "reject_mailbox_input",
        })?;
    let message = state_db
        .thread_queue()
        .reject_mail(
            params.receiver_thread_id,
            &params.message_id,
            &params.reason,
        )
        .await
        .map_err(|error| storage_error(error.as_ref()))?;
    decode_message(message)
}

pub(super) async fn accept(
    store: &LocalThreadStore,
    params: AcceptMailboxInputParams,
) -> ThreadStoreResult<StoredMailboxInput> {
    let authority = (params.final_subscription == crate::MailboxFinalSubscriptionRequest::Wake)
        .then_some(crate::MailboxFinalSubscriptionAuthority {
            receiver_lifecycle_epoch: 0,
            sender_lifecycle_epoch: 0,
        });
    accept_with_authority(store, params, authority).await
}

pub(super) async fn accept_with_authority(
    store: &LocalThreadStore,
    params: AcceptMailboxInputParams,
    authority: Option<crate::MailboxFinalSubscriptionAuthority>,
) -> ThreadStoreResult<StoredMailboxInput> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "accept_mailbox_input",
        })?;
    let payload = encode_payload(&params)?;
    let message = state_db
        .thread_queue()
        .accept_mail_with_final_subscription_and_authority(
            params.receiver_thread_id,
            &params.submission_key,
            &params.payload.sender().key(),
            &payload,
            params.final_subscription,
            authority,
        )
        .await
        .map_err(|error| storage_error(error.as_ref()))?;
    decode_message(message)
}

pub(super) async fn lookup_active_final_subscription(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
    sender: codex_protocol::ThreadId,
) -> ThreadStoreResult<Option<crate::MailboxFinalSubscription>> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "lookup_active_mailbox_final_subscription",
        })?;
    state_db
        .thread_queue()
        .read_active_mailbox_final_subscription(receiver, sender)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn lookup_final_subscription(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
    message_id: &str,
) -> ThreadStoreResult<Option<crate::MailboxFinalSubscription>> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "lookup_mailbox_final_subscription",
        })?;
    state_db
        .thread_queue()
        .read_mailbox_final_subscription(receiver, message_id)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn active_final_subscriptions_for_thread(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<Vec<crate::MailboxFinalSubscription>> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "read_active_mailbox_final_subscriptions_for_thread",
        })?;
    state_db
        .thread_queue()
        .read_active_mailbox_final_subscriptions_for_thread(thread_id)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn supersede_final_subscription(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
    sender: codex_protocol::ThreadId,
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "supersede_mailbox_final_subscription",
        })?;
    state_db
        .thread_queue()
        .supersede_mailbox_final_subscription(receiver, sender)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn supersede_final_subscription_message(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
    message_id: &str,
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "supersede_mailbox_final_subscription_message",
        })?;
    state_db
        .thread_queue()
        .supersede_mailbox_final_subscription_message(receiver, message_id)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn supersede_final_subscriptions_for_threads(
    store: &LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "supersede_mailbox_final_subscriptions_for_threads",
        })?;
    state_db
        .thread_queue()
        .supersede_mailbox_final_subscriptions_for_threads(thread_ids)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn acknowledge_final_subscription_delivery(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
    message_id: String,
    turn_id: String,
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "acknowledge_mailbox_final_subscription_delivery",
        })?;
    state_db
        .thread_queue()
        .acknowledge_mailbox_final_subscription_delivery(receiver, &message_id, &turn_id)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn claim(
    store: &LocalThreadStore,
    params: ClaimMailboxInputParams,
) -> ThreadStoreResult<MailboxClaim> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "claim_mailbox_input",
        })?;
    let claim = state_db
        .thread_queue()
        .claim_mail(&params.invocation, &params.selection.to_state())
        .await
        .map_err(|error| storage_error(error.as_ref()))?;
    decode_claim(claim)
}

pub(super) async fn reconcile(
    store: &LocalThreadStore,
    params: ReconcileMailboxDeliveryParams,
) -> ThreadStoreResult<MailboxClaim> {
    let current = claim(store, params.claim.clone()).await?;
    // Validate even terminal-member evidence before returning it unchanged.
    crate::mailbox_artifacts::verified_deliveries(&current, &params.deliveries, &[])?;
    if params.deliveries.is_empty()
        || current.messages.iter().all(|member| {
            matches!(
                member.message.state,
                crate::MailboxMessageState::Consumed | crate::MailboxMessageState::Rejected
            )
        })
    {
        return Ok(current);
    }
    let history = load_history(store, current.invocation.receiver_thread_id).await?;
    let verified =
        crate::mailbox_artifacts::verified_deliveries(&current, &params.deliveries, &history)?;
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "reconcile_mailbox_delivery",
        })?;
    for (message_id, delivery_id) in verified {
        state_db
            .thread_queue()
            .acknowledge_mail(&current.invocation, &message_id, &delivery_id)
            .await
            .map_err(|error| storage_error(error.as_ref()))?;
    }
    claim(store, params.claim).await
}

pub(super) async fn load_history(
    store: &LocalThreadStore,
    receiver: codex_protocol::ThreadId,
) -> ThreadStoreResult<Vec<codex_rollout::RolloutItem>> {
    let history = super::completion_artifacts::load_canonical_items(
        store,
        receiver,
        /*include_archived*/ true,
        super::completion_artifacts::CanonicalHistoryReadPolicy::RequireCompleteHistory,
    )
    .await?;
    // Read only the receiver's physical rollout, never expanded fork lineage.
    if !history.is_empty()
        && !matches!(history.first(), Some(codex_rollout::RolloutItem::SessionMeta(meta))
            if meta.meta.id == receiver)
    {
        return Err(ThreadStoreError::Internal {
            message: "mailbox delivery history belongs to another receiver".to_string(),
        });
    }
    Ok(history)
}
