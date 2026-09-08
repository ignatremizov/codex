//! Historical delivery proof, not effective model-context reconstruction.
//!
//! Rollback and compaction do not undo delivery. The caller supplies only the
//! receiver's own canonical history, never a fork's expanded source lineage.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::ResponseItemId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::RolloutItem;

use crate::MailboxClaim;
use crate::MailboxDeliveryArtifacts;
use crate::MailboxDeliveryEvidence;
use crate::MailboxMessageState;
use crate::MailboxPayload;
use crate::RecoveredMailboxClaim;
use crate::RecoveredMailboxDelivery;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::mailbox::invalid_request;
use crate::mailbox::storage_error;

/// One historical scan shared by recovery, checked acknowledgement, and revert.
pub(crate) fn recover_claims(
    claims: &[MailboxClaim],
    history: &[RolloutItem],
) -> ThreadStoreResult<Vec<RecoveredMailboxClaim>> {
    let mut indexed = HashMap::new();
    for (claim_index, claim) in claims.iter().enumerate() {
        for (member_index, member) in claim.messages.iter().enumerate() {
            if member.message.state != MailboxMessageState::Claimed {
                continue;
            }
            let id = codex_protocol::mailbox_delivery_response_item_id(&member.delivery_id)
                .ok_or_else(|| invalid_request("invalid durable mailbox delivery ID"))?;
            if indexed
                .insert(id.to_string(), (claim_index, member_index))
                .is_some()
            {
                return Err(invalid_request("duplicate durable mailbox delivery ID"));
            }
        }
    }
    let mut contexts: HashMap<(usize, usize), &ResponseItemEnvelope> = HashMap::new();
    let mut completions: HashMap<(usize, usize), (&ItemCompletedEvent, serde_json::Value)> =
        HashMap::new();
    for item in history {
        let id = match item {
            RolloutItem::ResponseItem(envelope) => envelope.id().map(ToString::to_string),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => Some(event.item.id()),
            _ => None,
        };
        let Some((claim_index, member_index)) = id.as_ref().and_then(|id| indexed.get(id)) else {
            continue;
        };
        let claim = &claims[*claim_index];
        let member = &claim.messages[*member_index];
        let key = (*claim_index, *member_index);
        match item {
            RolloutItem::ResponseItem(envelope) => {
                if !matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
                    || envelope
                        .item
                        .turn_id()
                        .is_some_and(|turn_id| turn_id != claim.invocation.turn_id)
                    || contexts
                        .get(&key)
                        .is_some_and(|previous| previous != &envelope)
                {
                    return Err(recovery_required(claim, &member.message.id));
                }
                contexts.insert(key, envelope);
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                let payload_matches = match (&member.message.payload, &event.item) {
                    (MailboxPayload::User { input, client_id }, TurnItem::UserMessage(message)) => {
                        &message.content == input && &message.client_id == client_id
                    }
                    (
                        MailboxPayload::Agent { input, attribution },
                        TurnItem::AgentMessage(message),
                    ) => {
                        message.input.as_ref() == Some(input)
                            && message.attribution.as_ref() == Some(attribution.as_ref())
                    }
                    _ => false,
                };
                let value = serde_json::to_value(event).map_err(|error| storage_error(&error))?;
                if !payload_matches
                    || event.thread_id != claim.invocation.receiver_thread_id
                    || event.turn_id != claim.invocation.turn_id
                    || completions
                        .get(&key)
                        .is_some_and(|(_, previous)| previous != &value)
                {
                    return Err(recovery_required(claim, &member.message.id));
                }
                completions.insert(key, (event, value));
            }
            _ => {}
        }
    }
    Ok(claims
        .iter()
        .enumerate()
        .map(|(claim_index, claim)| {
            let deliveries = claim
                .messages
                .iter()
                .enumerate()
                .map(|(member_index, member)| {
                    let key = (claim_index, member_index);
                    let context = contexts.remove(&key).cloned();
                    let completion = completions.remove(&key).map(|(event, _)| event.clone());
                    let artifacts = match (context, completion) {
                        (None, None) => MailboxDeliveryArtifacts::NotRecorded,
                        (Some(prepared_context), None) => {
                            MailboxDeliveryArtifacts::ContextOnly { prepared_context }
                        }
                        (None, Some(completion)) => {
                            MailboxDeliveryArtifacts::PresentationOnly { completion }
                        }
                        (Some(prepared_context), Some(completion)) => {
                            MailboxDeliveryArtifacts::Complete {
                                prepared_context,
                                completion,
                            }
                        }
                    };
                    RecoveredMailboxDelivery {
                        message_id: member.message.id.clone(),
                        artifacts,
                    }
                })
                .collect();
            RecoveredMailboxClaim {
                claim: claim.clone(),
                deliveries,
            }
        })
        .collect())
}

pub(crate) fn recovery_required(claim: &MailboxClaim, detail: &str) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: format!(
            "mailbox recovery required for receiver {}: partial or conflicting evidence ({detail}); \
             original rollout retained, claim is not permission to resend",
            claim.invocation.receiver_thread_id
        ),
    }
}

/// Validates the entire evidence set before identifying any acknowledgements.
pub(crate) fn verified_deliveries(
    claim: &MailboxClaim,
    evidence: &[MailboxDeliveryEvidence],
    history: &[RolloutItem],
) -> ThreadStoreResult<Vec<(String, String)>> {
    let members: HashMap<_, _> = claim
        .messages
        .iter()
        .map(|member| (member.message.id.as_str(), member))
        .collect();
    let mut seen = HashSet::new();
    let mut expected = HashMap::new();
    for item in evidence {
        if !seen.insert(item.message_id.as_str()) {
            return Err(invalid_request("duplicate mailbox delivery evidence"));
        }
        let member = members
            .get(item.message_id.as_str())
            .ok_or_else(|| invalid_request("mailbox delivery evidence is outside the claim"))?;
        let response_id = ResponseItemId::with_suffix("msg_mailbox", &member.delivery_id);
        if !matches!(
            &item.prepared_context.item,
            ResponseItem::Message { id: Some(id), role, .. }
                if id == &response_id && role == "user"
        ) {
            return Err(invalid_request(
                "mailbox delivery evidence must be a user Message with its reserved ID",
            ));
        }
        expected.insert(response_id.to_string(), (member, item));
    }
    let recovered = recover_claims(std::slice::from_ref(claim), history)?;
    let mut verified = Vec::new();
    for recovered in recovered {
        for delivery in recovered.deliveries {
            let member = members[delivery.message_id.as_str()];
            let id = ResponseItemId::with_suffix("msg_mailbox", &member.delivery_id);
            if let Some((_, evidence)) = expected.get(id.as_str())
                && let MailboxDeliveryArtifacts::Complete {
                    prepared_context, ..
                } = delivery.artifacts
                && prepared_context == evidence.prepared_context
            {
                verified.push((member.message.id.clone(), member.delivery_id.clone()));
            }
        }
    }
    Ok(verified)
}

#[cfg(test)]
#[path = "mailbox_artifacts_tests.rs"]
mod tests;
