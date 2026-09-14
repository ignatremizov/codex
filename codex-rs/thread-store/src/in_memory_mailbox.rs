//! Process-local parity for SQL acceptance and claims, not restart durability.

use codex_protocol::ThreadId;
use codex_state::MailboxClaimedMessage;
use codex_state::MailboxFinalSubscription;
use codex_state::MailboxFinalSubscriptionAuthority;
use codex_state::MailboxFinalSubscriptionRequest;
use codex_state::MailboxFinalSubscriptionState;
use codex_state::MailboxMessage;
use codex_state::MailboxMessageState;
use codex_state::MailboxSelection;

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
use crate::mailbox::invalid_request;

#[path = "in_memory_mailbox_inventory.rs"]
mod inventory;

/// Guarded by one mutex so acceptance and fixed-batch reservation are serialized.
#[derive(Default)]
pub(crate) struct InMemoryMailbox {
    messages: Vec<MailboxMessage>,
    claims: Vec<codex_state::MailboxClaim>,
    inventory: inventory::InventoryState,
}

impl InMemoryMailbox {
    pub(crate) fn lookup(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
    ) -> ThreadStoreResult<Option<StoredMailboxInput>> {
        self.messages
            .iter()
            .find(|message| {
                message.receiver_thread_id == receiver_thread_id
                    && message.submission_key == submission_key
            })
            .cloned()
            .map(decode_message)
            .transpose()
    }

    pub(crate) fn lookup_active_final_subscription(
        &self,
        receiver: ThreadId,
        sender: ThreadId,
    ) -> Option<MailboxFinalSubscription> {
        self.messages
            .iter()
            .filter(|message| message.receiver_thread_id == receiver)
            .filter_map(|message| message.final_subscription.as_ref())
            .find(|subscription| {
                subscription.sender_thread_id == sender
                    && matches!(
                        subscription.state,
                        MailboxFinalSubscriptionState::Pending
                            | MailboxFinalSubscriptionState::Bound
                    )
            })
            .cloned()
    }

    pub(crate) fn lookup_final_subscription(
        &self,
        receiver: ThreadId,
        message_id: &str,
    ) -> Option<MailboxFinalSubscription> {
        self.messages
            .iter()
            .find(|message| message.receiver_thread_id == receiver && message.id == message_id)
            .and_then(|message| message.final_subscription.clone())
    }

    pub(crate) fn active_final_subscriptions_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<MailboxFinalSubscription> {
        self.messages
            .iter()
            .filter_map(|message| message.final_subscription.as_ref())
            .filter(|subscription| {
                (subscription.receiver_thread_id == thread_id
                    || subscription.sender_thread_id == thread_id)
                    && matches!(
                        subscription.state,
                        MailboxFinalSubscriptionState::Pending
                            | MailboxFinalSubscriptionState::Bound
                    )
            })
            .cloned()
            .collect()
    }

    pub(crate) fn supersede_final_subscription(&mut self, receiver: ThreadId, sender: ThreadId) {
        for message in &mut self.messages {
            if message.receiver_thread_id == receiver
                && let Some(subscription) = message.final_subscription.as_mut()
                && subscription.sender_thread_id == sender
                && matches!(
                    subscription.state,
                    MailboxFinalSubscriptionState::Pending | MailboxFinalSubscriptionState::Bound
                )
            {
                subscription.state = MailboxFinalSubscriptionState::Superseded;
            }
        }
    }

    pub(crate) fn supersede_final_subscription_message(
        &mut self,
        receiver: ThreadId,
        message_id: &str,
    ) {
        if let Some(subscription) = self
            .messages
            .iter_mut()
            .find(|message| message.receiver_thread_id == receiver && message.id == message_id)
            .and_then(|message| message.final_subscription.as_mut())
            && matches!(
                subscription.state,
                MailboxFinalSubscriptionState::Pending | MailboxFinalSubscriptionState::Bound
            )
        {
            subscription.state = MailboxFinalSubscriptionState::Superseded;
        }
    }

    pub(crate) fn supersede_final_subscriptions_for_threads(&mut self, thread_ids: &[ThreadId]) {
        for thread_id in thread_ids {
            for message in &mut self.messages {
                if (message.receiver_thread_id == *thread_id
                    || message
                        .final_subscription
                        .as_ref()
                        .is_some_and(|subscription| subscription.sender_thread_id == *thread_id))
                    && let Some(subscription) = message.final_subscription.as_mut()
                    && matches!(
                        subscription.state,
                        MailboxFinalSubscriptionState::Pending
                            | MailboxFinalSubscriptionState::Bound
                    )
                {
                    subscription.state = MailboxFinalSubscriptionState::Superseded;
                }
            }
        }
    }

    pub(crate) fn acknowledge_final_subscription_delivery(
        &mut self,
        receiver: ThreadId,
        message_id: &str,
        turn_id: &str,
    ) -> ThreadStoreResult<()> {
        let subscription = self
            .messages
            .iter_mut()
            .find(|message| message.receiver_thread_id == receiver && message.id == message_id)
            .and_then(|message| message.final_subscription.as_mut())
            .ok_or_else(|| invalid_request("mailbox final subscription does not exist"))?;
        match subscription.state {
            MailboxFinalSubscriptionState::Bound
                if subscription.bound_turn_id.as_deref() == Some(turn_id) =>
            {
                subscription.state = MailboxFinalSubscriptionState::Delivered;
                Ok(())
            }
            MailboxFinalSubscriptionState::Delivered
                if subscription.bound_turn_id.as_deref() == Some(turn_id) =>
            {
                Ok(())
            }
            MailboxFinalSubscriptionState::Superseded
                if subscription.bound_turn_id.as_deref() == Some(turn_id) =>
            {
                Ok(())
            }
            _ => Err(invalid_request(
                "mailbox final subscription delivery does not match its bound turn",
            )),
        }
    }

    pub(crate) fn recover(
        &mut self,
        params: ClaimMailboxInputParams,
        history: Option<&[codex_rollout::RolloutItem]>,
    ) -> ThreadStoreResult<crate::RecoveredMailboxClaim> {
        let current = self.claim(params)?;
        let history = if current
            .messages
            .iter()
            .any(|member| member.message.state == MailboxMessageState::Claimed)
        {
            history.ok_or(ThreadStoreError::ThreadNotFound {
                thread_id: current.invocation.receiver_thread_id,
            })?
        } else {
            &[]
        };
        crate::mailbox_artifacts::recover_claims(&[current], history)?
            .pop()
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "mailbox recovery did not return its claim".to_string(),
            })
    }

    pub(crate) fn reject(
        &mut self,
        params: crate::RejectMailboxInputParams,
    ) -> ThreadStoreResult<StoredMailboxInput> {
        if params.reason.is_empty() {
            return Err(invalid_request("mailbox rejection reason must be nonempty"));
        }
        let message = self
            .messages
            .iter_mut()
            .find(|message| {
                message.receiver_thread_id == params.receiver_thread_id
                    && message.id == params.message_id
            })
            .ok_or_else(|| invalid_request("mailbox message not found"))?;
        match message.state {
            MailboxMessageState::Pending | MailboxMessageState::Claimed => {
                message.state = MailboxMessageState::Rejected;
                message.rejection_reason = Some(params.reason);
                if let Some(subscription) = message.final_subscription.as_mut()
                    && subscription.state == MailboxFinalSubscriptionState::Pending
                {
                    subscription.state = MailboxFinalSubscriptionState::Rejected;
                }
            }
            MailboxMessageState::Rejected
                if message.rejection_reason.as_deref() == Some(params.reason.as_str()) => {}
            MailboxMessageState::Consumed | MailboxMessageState::Rejected => {
                return Err(invalid_request(
                    "mailbox terminal outcome cannot be rewritten",
                ));
            }
        }
        decode_message(message.clone())
    }

    pub(crate) fn reconcile(
        &mut self,
        params: ReconcileMailboxDeliveryParams,
        history: Option<&[codex_rollout::RolloutItem]>,
    ) -> ThreadStoreResult<MailboxClaim> {
        let current = self.claim(params.claim.clone())?;
        crate::mailbox_artifacts::verified_deliveries(&current, &params.deliveries, &[])?;
        if params.deliveries.is_empty()
            || current.messages.iter().all(|member| {
                matches!(
                    member.message.state,
                    MailboxMessageState::Consumed | MailboxMessageState::Rejected
                )
            })
        {
            return Ok(current);
        }
        let history = history.ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: current.invocation.receiver_thread_id,
        })?;
        let verified =
            crate::mailbox_artifacts::verified_deliveries(&current, &params.deliveries, history)?;
        for (message_id, _) in verified {
            let message = self
                .messages
                .iter_mut()
                .find(|message| message.id == message_id)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "mailbox claim member is missing".to_string(),
                })?;
            message.state = MailboxMessageState::Consumed;
            if let Some(subscription) = message.final_subscription.as_mut()
                && subscription.state == MailboxFinalSubscriptionState::Pending
            {
                subscription.state = MailboxFinalSubscriptionState::Bound;
                subscription.bound_turn_id = Some(params.claim.invocation.turn_id.clone());
            }
        }
        self.claim(params.claim)
    }

    pub(crate) fn accept(
        &mut self,
        params: AcceptMailboxInputParams,
    ) -> ThreadStoreResult<StoredMailboxInput> {
        let authority = (params.final_subscription == MailboxFinalSubscriptionRequest::Wake)
            .then_some(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch: 0,
                sender_lifecycle_epoch: 0,
            });
        self.accept_with_authority(params, authority)
    }

    pub(crate) fn accept_with_authority(
        &mut self,
        params: AcceptMailboxInputParams,
        authority: Option<MailboxFinalSubscriptionAuthority>,
    ) -> ThreadStoreResult<StoredMailboxInput> {
        let payload_json = encode_payload(&params)?;
        let sender_key = params.payload.sender().key();
        let wants_final_subscription =
            params.final_subscription == MailboxFinalSubscriptionRequest::Wake;
        let sender_thread_id = match params.final_subscription {
            MailboxFinalSubscriptionRequest::None => None,
            MailboxFinalSubscriptionRequest::Wake => match &params.payload {
                crate::MailboxPayload::Agent { attribution, .. } => {
                    Some(attribution.sender.thread_id)
                }
                crate::MailboxPayload::User { .. } => {
                    return Err(invalid_request(
                        "mailbox final subscriptions require a canonical agent sender",
                    ));
                }
            },
        };
        let final_subscription_authority = match (params.final_subscription, authority) {
            (MailboxFinalSubscriptionRequest::None, None) => None,
            (MailboxFinalSubscriptionRequest::Wake, Some(authority))
                if authority.receiver_lifecycle_epoch >= 0
                    && authority.sender_lifecycle_epoch >= 0 =>
            {
                Some(authority)
            }
            _ => {
                return Err(invalid_request(
                    "mailbox final subscriptions require nonnegative endpoint authority epochs",
                ));
            }
        };
        if let Some(existing) = self.messages.iter().find(|message| {
            message.receiver_thread_id == params.receiver_thread_id
                && message.submission_key == params.submission_key
        }) {
            if existing.sender_key != sender_key
                || existing.payload_json != payload_json
                || existing.final_subscription.is_some() != wants_final_subscription
            {
                return Err(invalid_request(
                    "mailbox submission key already has different content or final subscription intent",
                ));
            }
            return decode_message(existing.clone());
        }
        let acceptance_sequence = i64::try_from(self.messages.len())
            .ok()
            .and_then(|sequence| sequence.checked_add(1))
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "mailbox acceptance sequence exhausted".to_string(),
            })?;
        let id = ThreadId::new().to_string();
        let final_subscription =
            sender_thread_id.map(|sender_thread_id| MailboxFinalSubscription {
                message_id: id.clone(),
                receiver_thread_id: params.receiver_thread_id,
                sender_thread_id,
                acceptance_sequence,
                state: MailboxFinalSubscriptionState::Pending,
                bound_turn_id: None,
                receiver_lifecycle_epoch: final_subscription_authority
                    .map(|authority| authority.receiver_lifecycle_epoch)
                    .unwrap_or_default(),
                sender_lifecycle_epoch: final_subscription_authority
                    .map(|authority| authority.sender_lifecycle_epoch)
                    .unwrap_or_default(),
            });
        let message = MailboxMessage {
            id,
            receiver_thread_id: params.receiver_thread_id,
            submission_key: params.submission_key,
            sender_key: sender_key.clone(),
            payload_json,
            acceptance_sequence,
            state: MailboxMessageState::Pending,
            rejection_reason: None,
            final_subscription,
        };
        let result = decode_message(message.clone())?;
        if sender_thread_id.is_some() {
            for previous in &mut self.messages {
                if previous.receiver_thread_id == params.receiver_thread_id
                    && previous.sender_key == sender_key
                    && let Some(subscription) = previous.final_subscription.as_mut()
                    && matches!(
                        subscription.state,
                        MailboxFinalSubscriptionState::Pending
                            | MailboxFinalSubscriptionState::Bound
                    )
                {
                    subscription.state = MailboxFinalSubscriptionState::Superseded;
                }
            }
        }
        self.messages.push(message);
        Ok(result)
    }

    pub(crate) fn lookup_claim(
        &self,
        invocation: &crate::MailboxInvocation,
    ) -> ThreadStoreResult<Option<MailboxClaim>> {
        if invocation.turn_id.is_empty() || invocation.tool_call_id.is_empty() {
            return Err(invalid_request(
                "mailbox invocation turn and tool call IDs must be nonempty",
            ));
        }
        self.claims
            .iter()
            .find(|claim| &claim.invocation == invocation)
            .cloned()
            .map(|mut claim| {
                for member in &mut claim.messages {
                    let message = self
                        .messages
                        .iter()
                        .find(|message| message.id == member.message.id)
                        .ok_or_else(|| ThreadStoreError::Internal {
                            message: "mailbox claim member is missing".to_string(),
                        })?;
                    member.message = message.clone();
                }
                decode_claim(claim)
            })
            .transpose()
    }

    pub(crate) fn claim(
        &mut self,
        params: ClaimMailboxInputParams,
    ) -> ThreadStoreResult<MailboxClaim> {
        if params.invocation.turn_id.is_empty() || params.invocation.tool_call_id.is_empty() {
            return Err(invalid_request(
                "mailbox invocation turn and tool call IDs must be nonempty",
            ));
        }
        let selection = params.selection.to_state();
        if let Some(existing) = self.lookup_claim(&params.invocation)? {
            if existing.selection.to_state() != selection {
                return Err(invalid_request(
                    "mailbox invocation already has a different sender selection",
                ));
            }
            return Ok(existing);
        }
        let mut messages = Vec::new();
        for message in &mut self.messages {
            let selected = match &selection {
                MailboxSelection::All => true,
                MailboxSelection::Senders(senders) => senders.contains(&message.sender_key),
            };
            if message.receiver_thread_id == params.invocation.receiver_thread_id
                && message.state == MailboxMessageState::Pending
                && selected
            {
                message.state = MailboxMessageState::Claimed;
                messages.push(MailboxClaimedMessage {
                    message: message.clone(),
                    delivery_id: ThreadId::new().to_string(),
                });
            }
        }
        let claim = codex_state::MailboxClaim {
            invocation: params.invocation,
            selection,
            messages,
        };
        let result = decode_claim(claim.clone())?;
        self.claims.push(claim);
        Ok(result)
    }
}
