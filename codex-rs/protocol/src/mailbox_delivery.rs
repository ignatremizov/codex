//! Stable identities for trusted mailbox delivery artifacts.
//!
//! Identity recognition is not delivery proof or authorization. The store must
//! also verify the accepted payload and fully qualified invocation.

use crate::ResponseItemId;
use crate::items::TurnItem;
use crate::protocol::ItemCompletedEvent;

const MAILBOX_DELIVERY_PREFIX: &str = "msg_mailbox_";

/// Stable identity for a live, presentation-only notice of durable mailbox acceptance.
/// This is neither payload delivery proof nor a guarantee that a client received the notice.
pub fn mailbox_acceptance_receipt_id(message_id: &str) -> Option<ResponseItemId> {
    mailbox_delivery_response_item_id(message_id)?;
    Some(ResponseItemId::with_suffix(
        "msg_mailbox_accepted",
        message_id,
    ))
}

/// Recognizes trusted acceptance notices, distinct from inventory and consumption.
pub fn is_mailbox_acceptance_receipt_id(id: &str) -> bool {
    id.strip_prefix("msg_mailbox_accepted_")
        .and_then(mailbox_acceptance_receipt_id)
        .is_some()
}

/// Builds the paired context/presentation ID from a canonical UUIDv7 delivery ID.
pub fn mailbox_delivery_response_item_id(delivery_id: &str) -> Option<ResponseItemId> {
    let uuid = uuid::Uuid::parse_str(delivery_id).ok()?;
    (uuid.get_version() == Some(uuid::Version::SortRand) && uuid.to_string() == delivery_id)
        .then(|| ResponseItemId::with_suffix("msg_mailbox", delivery_id))
}

/// Recognizes the reserved identity; provider-authored IDs must be sanitized separately.
pub fn is_mailbox_delivery_response_item_id(id: &str) -> bool {
    id.strip_prefix(MAILBOX_DELIVERY_PREFIX)
        .and_then(mailbox_delivery_response_item_id)
        .is_some()
}

/// Stable inventory context identity, separate from payload delivery identities.
/// The notification UUID also binds the inventory's turn.
pub fn mailbox_inventory_response_item_id(notification_id: &str) -> Option<ResponseItemId> {
    mailbox_delivery_response_item_id(notification_id)?;
    Some(ResponseItemId::with_suffix(
        "msg_mailbox_inventory",
        notification_id,
    ))
}

/// Recognizes reserved inventory IDs for trusted canonical context.
pub fn is_mailbox_inventory_response_item_id(id: &str) -> bool {
    id.strip_prefix("msg_mailbox_inventory_")
        .and_then(mailbox_inventory_response_item_id)
        .is_some()
}

/// Recognizes typed mailbox input for durable retention, not consumption acknowledgement.
///
/// Ordinary assistant output is insufficient: agent mail must retain original
/// input and trusted attribution to the receiving thread.
pub fn is_mailbox_delivery_completion(event: &ItemCompletedEvent) -> bool {
    if !is_mailbox_delivery_response_item_id(&event.item.id()) {
        return false;
    }
    match &event.item {
        TurnItem::UserMessage(_) => true,
        TurnItem::AgentMessage(item) => {
            item.input.is_some()
                && item
                    .attribution
                    .as_ref()
                    .is_some_and(|attribution| attribution.recipient.thread_id == event.thread_id)
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "mailbox_delivery_tests.rs"]
mod tests;
