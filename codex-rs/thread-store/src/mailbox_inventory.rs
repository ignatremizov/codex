//! Inventory progress is notification state, never payload consumption or authority.

use codex_protocol::ThreadId;
use codex_protocol::mailbox_inventory_response_item_id;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout::ResponseItemEnvelope;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::MailboxSender;
use crate::ThreadStoreResult;
use crate::mailbox::invalid_request;
use crate::mailbox::sender_from_key;
use crate::mailbox::storage_error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxSenderInventory {
    pub sender: MailboxSender,
    pub count: u64,
    pub max_acceptance_sequence: i64,
}

/// Fixed snapshot; its UUID is also its immutable inventory turn ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventoryNotification {
    pub id: String,
    pub receiver_thread_id: ThreadId,
    pub through_sequence: i64,
    pub pending_senders: Vec<MailboxSenderInventory>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventory {
    pub receiver_thread_id: ThreadId,
    pub notified_through: i64,
    pub pending_senders: Vec<MailboxSenderInventory>,
    pub claimed_senders: Vec<MailboxSenderInventory>,
    pub active_notification: Option<MailboxInventoryNotification>,
}

/// Recovery never authorizes a wake, payload insertion, or cancellation.
#[derive(Clone, Debug, PartialEq)]
pub enum MailboxInventoryRecovery {
    NotRecorded,
    Recorded {
        context: ResponseItemEnvelope,
    },
    AlreadyCovered {
        context: ResponseItemEnvelope,
        notified_through: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxInventoryAcknowledgementOutcome {
    Acknowledged,
    AlreadyCovered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventoryAcknowledgement {
    pub notification_id: String,
    pub notified_through: i64,
    pub outcome: MailboxInventoryAcknowledgementOutcome,
}

#[derive(Serialize)]
struct InventoryContext<'a> {
    notification_id: &'a str,
    receiver_thread_id: ThreadId,
    through_sequence: i64,
    pending_senders: Vec<InventoryContextSender>,
}

#[derive(Serialize)]
struct InventoryContextSender {
    sender_key: String,
    count: u64,
    max_acceptance_sequence: i64,
}

// Canonical sender keys contain only `user` or a UUID, and numeric fields are
// fixed-width integers. The byte guard also fails closed if this format grows.
const MAX_INVENTORY_CONTEXT_SENDERS: usize = 8;
const MAX_INVENTORY_CONTEXT_BYTES: usize = 2_048;

#[cfg(test)]
#[path = "mailbox_inventory_context_tests.rs"]
mod context_tests;

#[derive(Serialize)]
struct BoundedInventoryContext<'a> {
    format_version: u8,
    notification_id: &'a str,
    receiver_thread_id: ThreadId,
    through_sequence: i64,
    snapshot_sha256: String,
    pending_senders: &'a [InventoryContextSender],
    omitted_sender_count: usize,
}

impl MailboxInventoryNotification {
    /// Builds deterministic harness context, not user task input.
    ///
    /// Contains only the frozen canonical snapshot and guidance, never mail
    /// payloads or volatile labels. Recovery returns existing stamps unchanged.
    /// This format is durable: future changes must retain recognition of
    /// already-recorded inventory contexts.
    pub fn context(&self) -> ThreadStoreResult<ResponseItemEnvelope> {
        let snapshot = self.validated_context_snapshot()?;
        let json = serde_json::to_string(&snapshot).map_err(|error| storage_error(&error))?;
        let json = if snapshot.pending_senders.len() > MAX_INVENTORY_CONTEXT_SENDERS {
            serde_json::to_string(&BoundedInventoryContext {
                format_version: 2,
                notification_id: &self.id,
                receiver_thread_id: self.receiver_thread_id,
                through_sequence: self.through_sequence,
                snapshot_sha256: format!("{:x}", Sha256::digest(json.as_bytes())),
                pending_senders: &snapshot.pending_senders[..MAX_INVENTORY_CONTEXT_SENDERS],
                omitted_sender_count: snapshot.pending_senders.len() - MAX_INVENTORY_CONTEXT_SENDERS,
            }).map_err(|error| storage_error(&error))?
        } else {
            json
        };
        let text = inventory_context_text(&json);
        if text.len() > MAX_INVENTORY_CONTEXT_BYTES {
            return Err(invalid_request("mailbox inventory context exceeded its hard byte limit"));
        }
        self.context_with_text(text)
    }

    /// Recognition only for an existing pre-bound context. Never use this method
    /// to publish fresh inventory; retained canonical bytes remain authoritative.
    pub(crate) fn legacy_context(&self) -> ThreadStoreResult<ResponseItemEnvelope> {
        let snapshot = self.validated_context_snapshot()?;
        let json = serde_json::to_string(&snapshot).map_err(|error| storage_error(&error))?;
        self.context_with_text(inventory_context_text(&json))
    }

    fn validated_context_snapshot(&self) -> ThreadStoreResult<InventoryContext<'_>> {
        mailbox_inventory_response_item_id(&self.id)
            .ok_or_else(|| invalid_request("invalid mailbox inventory notification ID"))?;
        let pending_senders = self
            .pending_senders
            .iter()
            .map(|group| InventoryContextSender {
                sender_key: group.sender.key(),
                count: group.count,
                max_acceptance_sequence: group.max_acceptance_sequence,
            })
            .collect::<Vec<_>>();
        if self.through_sequence <= 0
            || pending_senders.is_empty()
            || pending_senders.iter().any(|group| {
                group.count == 0
                    || group.max_acceptance_sequence <= 0
                    || group.max_acceptance_sequence > self.through_sequence
            })
            || pending_senders
                .windows(2)
                .any(|pair| pair[0].sender_key >= pair[1].sender_key)
            || pending_senders
                .iter()
                .map(|group| group.max_acceptance_sequence)
                .max()
                != Some(self.through_sequence)
        {
            return Err(invalid_request(
                "invalid fixed mailbox inventory groups or frontier",
            ));
        }
        Ok(InventoryContext {
            notification_id: &self.id,
            receiver_thread_id: self.receiver_thread_id,
            through_sequence: self.through_sequence,
            pending_senders,
        })
    }

    fn context_with_text(&self, text: String) -> ThreadStoreResult<ResponseItemEnvelope> {
        let id = mailbox_inventory_response_item_id(&self.id)
            .ok_or_else(|| invalid_request("invalid mailbox inventory notification ID"))?;
        let mut item = ResponseItem::Message {
            id: Some(id),
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing(&self.id);
        Ok(ResponseItemEnvelope::new(item))
    }
}

fn inventory_context_text(json: &str) -> String {
    format!(
        "Mailbox inventory (pending mail, not consumed):\n{json}\n\
         Use check_mail(from: ...) to consume a sender's mail: use \"user\" for user mail \
         or the UUID after \"agent:\" for agent mail. Use check_mail with no filter to consume all pending mail."
    )
}

fn decode_groups(
    groups: Vec<codex_state::MailboxSenderInventory>,
) -> ThreadStoreResult<Vec<MailboxSenderInventory>> {
    groups
        .into_iter()
        .map(|group| {
            Ok(MailboxSenderInventory {
                sender: sender_from_key(&group.sender_key)?,
                count: group.count,
                max_acceptance_sequence: group.max_acceptance_sequence,
            })
        })
        .collect()
}

pub(crate) fn decode_notification(
    notification: codex_state::MailboxInventoryNotification,
) -> ThreadStoreResult<MailboxInventoryNotification> {
    let notification = MailboxInventoryNotification {
        id: notification.id,
        receiver_thread_id: notification.receiver_thread_id,
        through_sequence: notification.through_sequence,
        pending_senders: decode_groups(notification.pending_senders)?,
    };
    notification.context()?;
    Ok(notification)
}

pub(crate) fn decode_inventory(
    inventory: codex_state::MailboxInventory,
) -> ThreadStoreResult<MailboxInventory> {
    Ok(MailboxInventory {
        receiver_thread_id: inventory.receiver_thread_id,
        notified_through: inventory.notified_through,
        pending_senders: decode_groups(inventory.pending_senders)?,
        claimed_senders: decode_groups(inventory.claimed_senders)?,
        active_notification: inventory
            .active_notification
            .map(decode_notification)
            .transpose()?,
    })
}
