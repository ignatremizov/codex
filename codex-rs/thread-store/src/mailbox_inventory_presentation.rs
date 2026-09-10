//! Recognition of stored inventory text for disposable presentation, never delivery proof.

use super::BoundedInventoryContext;
use super::InventoryContext;
use super::MAX_INVENTORY_CONTEXT_BYTES;
use super::MAX_INVENTORY_CONTEXT_SENDERS;
use super::MailboxInventoryNotification;
use super::MailboxSenderInventory;
use super::inventory_context_text;
use crate::mailbox::sender_from_key;
use codex_protocol::ThreadId;
use codex_protocol::mailbox_inventory_response_item_id;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

/// The sender prefix actually present in a recognized inventory context.
///
/// A bounded context cannot reconstruct its omitted senders or verify their commitment without
/// the original snapshot. This view never supplies that authority and must not be used for
/// acknowledgement, recovery, payload selection, or permission checks.
pub struct MailboxInventoryContextView {
    pub pending_senders: Vec<MailboxSenderInventory>,
    pub omitted: bool,
}

impl MailboxInventoryContextView {
    /// Recognizes the exact durable serializer and receiver/notification identity without I/O.
    /// Unknown or inconsistent input remains unchanged at the presentation boundary.
    pub fn from_item(item: &ResponseItem, receiver: ThreadId) -> Option<Self> {
        let ResponseItem::Message {
            id: Some(id),
            role,
            content,
            phase: None,
            ..
        } = item
        else {
            return None;
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return None;
        };
        if role != "developer" {
            return None;
        }
        let (body, _) = text
            .strip_prefix("Mailbox inventory (pending mail, not consumed):\n")?
            .split_once('\n')?;
        if let Ok(snapshot) = serde_json::from_str::<InventoryContext>(body) {
            if snapshot.receiver_thread_id != receiver
                || item.turn_id() != Some(snapshot.notification_id.as_str())
                || mailbox_inventory_response_item_id(&snapshot.notification_id).as_ref()
                    != Some(id)
            {
                return None;
            }
            let pending_senders = snapshot
                .pending_senders
                .into_iter()
                .map(|sender| {
                    Some(MailboxSenderInventory {
                        sender: sender_from_key(&sender.sender_key).ok()?,
                        count: sender.count,
                        max_acceptance_sequence: sender.max_acceptance_sequence,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            let notification = MailboxInventoryNotification {
                id: snapshot.notification_id,
                receiver_thread_id: receiver,
                through_sequence: snapshot.through_sequence,
                pending_senders,
            };
            // This also recognizes old full snapshots larger than today's fresh-context cap.
            let canonical = notification.legacy_context().ok()?;
            if !matches!(&canonical.item, ResponseItem::Message { content: expected, .. }
                if expected == content)
            {
                return None;
            }
            return Some(Self {
                pending_senders: notification.pending_senders,
                omitted: false,
            });
        }
        let snapshot = serde_json::from_str::<BoundedInventoryContext>(body).ok()?;
        if snapshot.format_version != 2
            || snapshot.receiver_thread_id != receiver
            || item.turn_id() != Some(snapshot.notification_id.as_str())
            || mailbox_inventory_response_item_id(&snapshot.notification_id).as_ref() != Some(id)
            || snapshot.through_sequence <= 0
            || snapshot.pending_senders.len() != MAX_INVENTORY_CONTEXT_SENDERS
            || snapshot.omitted_sender_count == 0
            || snapshot.snapshot_sha256.len() != 64
            || !snapshot
                .snapshot_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || snapshot.pending_senders.iter().any(|sender| {
                sender.count == 0
                    || sender.max_acceptance_sequence <= 0
                    || sender.max_acceptance_sequence > snapshot.through_sequence
            })
            || snapshot
                .pending_senders
                .windows(2)
                .any(|pair| pair[0].sender_key >= pair[1].sender_key)
            || text.len() > MAX_INVENTORY_CONTEXT_BYTES
        {
            return None;
        }
        let canonical = inventory_context_text(&serde_json::to_string(&snapshot).ok()?);
        if text != &canonical {
            return None;
        }
        let pending_senders = snapshot
            .pending_senders
            .into_iter()
            .map(|sender| {
                let parsed = sender_from_key(&sender.sender_key).ok()?;
                (parsed.key() == sender.sender_key).then_some(MailboxSenderInventory {
                    sender: parsed,
                    count: sender.count,
                    max_acceptance_sequence: sender.max_acceptance_sequence,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            pending_senders,
            omitted: true,
        })
    }
}

#[cfg(test)]
#[path = "mailbox_inventory_presentation_tests.rs"]
mod tests;
