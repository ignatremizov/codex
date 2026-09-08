//! Process-local parity for SQL acceptance and claims, not restart durability.

use codex_protocol::ThreadId;
use codex_state::MailboxClaimedMessage;
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
        }
        self.claim(params.claim)
    }

    pub(crate) fn accept(
        &mut self,
        params: AcceptMailboxInputParams,
    ) -> ThreadStoreResult<StoredMailboxInput> {
        let payload_json = encode_payload(&params)?;
        let sender_key = params.payload.sender().key();
        if let Some(existing) = self.messages.iter().find(|message| {
            message.receiver_thread_id == params.receiver_thread_id
                && message.submission_key == params.submission_key
        }) {
            if existing.sender_key != sender_key || existing.payload_json != payload_json {
                return Err(invalid_request(
                    "mailbox submission key already has different content",
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
        let message = MailboxMessage {
            id: ThreadId::new().to_string(),
            receiver_thread_id: params.receiver_thread_id,
            submission_key: params.submission_key,
            sender_key,
            payload_json,
            acceptance_sequence,
            state: MailboxMessageState::Pending,
            rejection_reason: None,
        };
        let result = decode_message(message.clone())?;
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
