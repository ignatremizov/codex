//! Reconcile committed delivery under the existing revert writer reservations.

use super::LocalThreadStore;
use crate::MailboxDeliveryArtifacts;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::mailbox::decode_claim;
use crate::mailbox::storage_error;
use crate::mailbox_artifacts::recover_claims;
use crate::mailbox_artifacts::recovery_required;
use codex_protocol::ThreadId;
use codex_rollout::RolloutRecorder;
use std::path::Path;

pub(super) async fn reconcile_before_revert(
    store: &LocalThreadStore,
    receiver: ThreadId,
    source_path: &Path,
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "revert_mailbox_recovery",
        })?;
    let claims = state_db
        .thread_queue()
        .list_unconsumed_mail_claims(receiver)
        .await
        .map_err(|error| storage_error(error.as_ref()))?
        .into_iter()
        .map(decode_claim)
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let inventory = super::mailbox_inventory::read(store, receiver).await?;
    if claims.is_empty() && inventory.active_notification.is_none() {
        return Ok(());
    }
    let (history, owner, parse_errors) =
        RolloutRecorder::load_rollout_items_for_mailbox_recovery(source_path)
            .await
            .map_err(|error| storage_error(&error))?;
    if owner != Some(receiver) || parse_errors != 0 {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "mailbox recovery required for receiver {receiver}: unreadable or foreign delivery history; \
                 original rollout retained, claims are not permission to resend"
            ),
        });
    }
    let recovered = recover_claims(&claims, &history)?;
    let inventory_ack = if let Some(notification) = inventory.active_notification.as_ref() {
        match crate::mailbox_inventory_artifacts::recover(notification, &inventory, &history)? {
            crate::MailboxInventoryRecovery::Recorded { .. } => Some(notification),
            // A preparation with no context survives revert; it does not block it.
            crate::MailboxInventoryRecovery::NotRecorded
            | crate::MailboxInventoryRecovery::AlreadyCovered { .. } => None,
        }
    } else {
        None
    };
    let mut acknowledgements = Vec::new();
    for recovered in &recovered {
        for (member, delivery) in recovered.claim.messages.iter().zip(&recovered.deliveries) {
            match &delivery.artifacts {
                MailboxDeliveryArtifacts::NotRecorded => {}
                MailboxDeliveryArtifacts::Complete { .. } => {
                    acknowledgements.push((&recovered.claim.invocation, member));
                }
                MailboxDeliveryArtifacts::ContextOnly { .. }
                | MailboxDeliveryArtifacts::PresentationOnly { .. } => {
                    return Err(recovery_required(&recovered.claim, &member.message.id));
                }
            }
        }
    }
    // Validate the whole snapshot before any acknowledgements. SQL and JSONL
    // remain separate commits; a later failure can leave valid acks committed.
    for (invocation, member) in acknowledgements {
        state_db
            .thread_queue()
            .acknowledge_mail(invocation, &member.message.id, &member.delivery_id)
            .await
            .map_err(|error| storage_error(error.as_ref()))?;
    }
    if let Some(notification) = inventory_ack {
        state_db
            .thread_queue()
            .acknowledge_mail_inventory(receiver, &notification.id)
            .await
            .map_err(|error| storage_error(error.as_ref()))?;
    }
    Ok(())
}
