//! Process-local inventory parity under the mailbox's existing mutex.

use super::InMemoryMailbox;
use crate::MailboxFinalSubscription;
use crate::MailboxFinalSubscriptionState;
use crate::MailboxInventory;
use crate::MailboxInventoryAcknowledgement;
use crate::MailboxInventoryAcknowledgementOutcome;
use crate::MailboxInventoryNotification;
use crate::MailboxInventoryRecovery;
use crate::MailboxMessageState;
use crate::MailboxSenderInventory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::mailbox::sender_from_key;
use crate::mailbox_inventory_artifacts::recovery_error;
use codex_protocol::ThreadId;
use codex_rollout::RolloutItem;
use std::collections::BTreeMap;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct InventoryState {
    progress: HashMap<ThreadId, InventoryProgress>,
}

#[derive(Default)]
struct InventoryProgress {
    notified_through: i64,
    active: Option<MailboxInventoryNotification>,
}

impl InMemoryMailbox {
    pub(crate) fn has_pending_inventory(
        &self,
        notification: &MailboxInventoryNotification,
    ) -> ThreadStoreResult<bool> {
        notification.context()?;
        Ok(self.messages.iter().any(|message| {
            message.receiver_thread_id == notification.receiver_thread_id
                && message.state == MailboxMessageState::Pending
                && message.acceptance_sequence <= notification.through_sequence
        }))
    }

    pub(crate) fn read_inventory(&self, receiver: ThreadId) -> ThreadStoreResult<MailboxInventory> {
        let mut pending = BTreeMap::new();
        let mut claimed = BTreeMap::new();
        for message in self
            .messages
            .iter()
            .filter(|message| message.receiver_thread_id == receiver)
        {
            let groups = match message.state {
                MailboxMessageState::Pending => &mut pending,
                MailboxMessageState::Claimed => &mut claimed,
                MailboxMessageState::Consumed | MailboxMessageState::Rejected => continue,
            };
            let group =
                groups
                    .entry(message.sender_key.clone())
                    .or_insert(MailboxSenderInventory {
                        sender: sender_from_key(&message.sender_key)?,
                        count: 0,
                        max_acceptance_sequence: 0,
                    });
            group.count += 1;
            group.max_acceptance_sequence = group
                .max_acceptance_sequence
                .max(message.acceptance_sequence);
        }
        let progress = self.inventory.progress.get(&receiver);
        Ok(MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: progress.map_or(0, |progress| progress.notified_through),
            pending_senders: pending.into_values().collect(),
            claimed_senders: claimed.into_values().collect(),
            active_notification: progress.and_then(|progress| progress.active.clone()),
        })
    }

    pub(crate) fn prepare_inventory(
        &mut self,
        receiver: ThreadId,
    ) -> ThreadStoreResult<Option<MailboxInventoryNotification>> {
        let inventory = self.read_inventory(receiver)?;
        if let Some(active) = inventory.active_notification {
            return Ok(Some(active));
        }
        let through = inventory
            .pending_senders
            .iter()
            .map(|group| group.max_acceptance_sequence)
            .max()
            .filter(|through| *through > inventory.notified_through);
        let Some(through_sequence) = through else {
            return Ok(None);
        };
        let notification = MailboxInventoryNotification {
            id: ThreadId::new().to_string(),
            receiver_thread_id: receiver,
            through_sequence,
            pending_senders: inventory.pending_senders,
        };
        self.inventory.progress.entry(receiver).or_default().active = Some(notification.clone());
        Ok(Some(notification))
    }

    pub(crate) fn recover_inventory(
        &self,
        notification: &MailboxInventoryNotification,
        history: Option<&[RolloutItem]>,
    ) -> ThreadStoreResult<MailboxInventoryRecovery> {
        let inventory = self.read_inventory(notification.receiver_thread_id)?;
        let history = history.ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: notification.receiver_thread_id,
        })?;
        crate::mailbox_inventory_artifacts::recover(notification, &inventory, history)
    }

    pub(crate) fn reconcile_inventory(
        &mut self,
        notification: MailboxInventoryNotification,
        history: Option<&[RolloutItem]>,
    ) -> ThreadStoreResult<MailboxInventoryAcknowledgement> {
        let (notified_through, outcome, bound_subscriptions) =
            match self.recover_inventory(&notification, history)? {
                MailboxInventoryRecovery::NotRecorded => return Err(recovery_error(&notification)),
                MailboxInventoryRecovery::Recorded { .. } => {
                    let notified_through = {
                        let progress = self
                            .inventory
                            .progress
                            .get_mut(&notification.receiver_thread_id)
                            .ok_or_else(|| recovery_error(&notification))?;
                        if progress.active.as_ref() != Some(&notification) {
                            return Err(recovery_error(&notification));
                        }
                        progress.notified_through =
                            progress.notified_through.max(notification.through_sequence);
                        progress.active = None;
                        progress.notified_through
                    };
                    let mut bound_subscriptions = Vec::new();
                    for message in &mut self.messages {
                        if message.receiver_thread_id == notification.receiver_thread_id
                            && message.state == MailboxMessageState::Pending
                            && message.acceptance_sequence <= notification.through_sequence
                            && let Some(subscription) = message.final_subscription.as_mut()
                            && subscription.state == MailboxFinalSubscriptionState::Pending
                        {
                            subscription.state = MailboxFinalSubscriptionState::Bound;
                            subscription.bound_turn_id = Some(notification.id.clone());
                            bound_subscriptions.push(subscription.clone());
                        }
                    }
                    (
                        notified_through,
                        MailboxInventoryAcknowledgementOutcome::Acknowledged,
                        bound_subscriptions,
                    )
                }
                MailboxInventoryRecovery::AlreadyCovered {
                    notified_through, ..
                } => {
                    let bound_subscriptions = self
                        .messages
                        .iter()
                        .filter_map(|message| message.final_subscription.as_ref())
                        .filter(|subscription| {
                            subscription.receiver_thread_id == notification.receiver_thread_id
                                && subscription.bound_turn_id.as_deref()
                                    == Some(notification.id.as_str())
                                && subscription.state == MailboxFinalSubscriptionState::Bound
                        })
                        .cloned()
                        .collect::<Vec<MailboxFinalSubscription>>();
                    (
                        notified_through,
                        MailboxInventoryAcknowledgementOutcome::AlreadyCovered,
                        bound_subscriptions,
                    )
                }
            };
        Ok(MailboxInventoryAcknowledgement {
            notification_id: notification.id,
            notified_through,
            outcome,
            bound_subscriptions,
        })
    }

    pub(crate) fn cancel_inventory(
        &mut self,
        notification: MailboxInventoryNotification,
        history: Option<&[RolloutItem]>,
    ) -> ThreadStoreResult<()> {
        if !matches!(
            self.recover_inventory(&notification, history)?,
            MailboxInventoryRecovery::NotRecorded
        ) {
            return Err(recovery_error(&notification));
        }
        let progress = self
            .inventory
            .progress
            .get_mut(&notification.receiver_thread_id)
            .ok_or_else(|| recovery_error(&notification))?;
        if progress.active.as_ref() != Some(&notification) {
            return Err(recovery_error(&notification));
        }
        progress.active = None;
        Ok(())
    }
}
