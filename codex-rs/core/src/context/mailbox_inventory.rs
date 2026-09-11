//! Disposable inventory presentation. Canonical snapshots remain recovery authority.

use super::ContextualUserFragment;
use codex_protocol::ThreadId;
use codex_protocol::is_mailbox_inventory_response_item_id;
use codex_protocol::mailbox_inventory_response_item_id;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_thread_store::MailboxInventoryNotification;
use codex_thread_store::MailboxSender;
use codex_thread_store::MailboxSenderInventory;
use codex_utils_string::approx_bytes_for_tokens;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

const INVENTORY_TOKENS: usize = 768;
const HEADING: &str = "Mailbox inventory (pending mail, not consumed):\n";
const GUIDANCE: &str = "\nMail is not consumed until you call check_mail. Pass a listed from value directly to check_mail(from: ...): \
    \"user\" for user mail, an advertised agent ref or a UUID for agent mail. \
    Refs use the accompanying identity mapping; nicknames are descriptive only. \
    Counts are a frozen inventory snapshot, not a guarantee for the next check_mail; later arrivals may change its result. \
    Call check_mail with no filter to consume all pending mail.";
const OMITTED: &str =
    " Additional pending senders are omitted; check_mail with no filter includes their mail.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalInventory {
    notification_id: String,
    receiver_thread_id: ThreadId,
    through_sequence: i64,
    pending_senders: Vec<CanonicalSender>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalSender {
    sender_key: String,
    count: u64,
    max_acceptance_sequence: i64,
}

#[derive(Serialize)]
struct PendingSender {
    from: String,
    count: u64,
}

struct MailboxInventoryContext {
    receiver: String,
    pending_senders: Vec<PendingSender>,
    omitted: bool,
}

impl ContextualUserFragment for MailboxInventoryContext {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.mailbox_inventory".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (HEADING, "")
    }

    fn body(&self) -> String {
        let snapshot = json!({
            "receiver": self.receiver,
            "pending_senders": self.pending_senders,
        });
        let omitted = if self.omitted { OMITTED } else { "" };
        format!("{snapshot}{GUIDANCE}{omitted}")
    }
}

/// Projects exact trusted inventory artifacts using only refs advertised in this
/// request's captured receiver namespace. Does not read live mail or acknowledge it.
pub(crate) fn project_mailbox_inventories(
    items: &mut [ResponseItem],
    receiver: ThreadId,
    refs: &HashMap<ThreadId, u64>,
) {
    let reference = |id: ThreadId| {
        refs.get(&id)
            .map(u64::to_string)
            .unwrap_or_else(|| id.to_string())
    };
    for item in items {
        let ResponseItem::Message {
            id: Some(id),
            role,
            content,
            phase: None,
            ..
        } = &*item
        else {
            continue;
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            continue;
        };
        if role != "developer" || !is_mailbox_inventory_response_item_id(id.as_str()) {
            continue;
        }
        let Some((body, _)) = text
            .strip_prefix(HEADING)
            .and_then(|text| text.split_once('\n'))
        else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_str::<CanonicalInventory>(body) else {
            continue;
        };
        if snapshot.receiver_thread_id != receiver
            || item.turn_id() != Some(snapshot.notification_id.as_str())
            || mailbox_inventory_response_item_id(&snapshot.notification_id).as_ref() != Some(id)
        {
            continue;
        }
        let groups = snapshot
            .pending_senders
            .into_iter()
            .map(|group| {
                let sender = if group.sender_key == "user" {
                    MailboxSender::User
                } else {
                    let id =
                        ThreadId::from_string(group.sender_key.strip_prefix("agent:")?).ok()?;
                    if format!("agent:{id}") != group.sender_key {
                        return None;
                    }
                    MailboxSender::Agent(id)
                };
                Some(MailboxSenderInventory {
                    sender,
                    count: group.count,
                    max_acceptance_sequence: group.max_acceptance_sequence,
                })
            })
            .collect::<Option<Vec<_>>>();
        let Some(pending_senders) = groups else {
            continue;
        };
        let notification = MailboxInventoryNotification {
            id: snapshot.notification_id,
            receiver_thread_id: receiver,
            through_sequence: snapshot.through_sequence,
            pending_senders,
        };
        // Reuse the durable format's validation, including sorted unique senders
        // and frontier consistency. Merely resembling its JSON is insufficient.
        let Ok(canonical) = notification.context() else {
            continue;
        };
        let ResponseItem::Message {
            content: canonical_content,
            ..
        } = &canonical.item
        else {
            continue;
        };
        if canonical_content != content {
            continue;
        }
        let mut fragment = MailboxInventoryContext {
            receiver: reference(receiver),
            pending_senders: Vec::new(),
            omitted: false,
        };
        let mut remaining = approx_bytes_for_tokens(INVENTORY_TOKENS)
            .saturating_sub(fragment.render().len() + OMITTED.len());
        for group in notification.pending_senders {
            let sender = PendingSender {
                from: match group.sender {
                    MailboxSender::User => "user".to_string(),
                    MailboxSender::Agent(id) => reference(id),
                },
                count: group.count,
            };
            let bytes = json!(sender).to_string().len() + 1;
            if bytes > remaining {
                fragment.omitted = true;
                break;
            }
            remaining -= bytes;
            fragment.pending_senders.push(sender);
        }
        if let ResponseItem::Message { content, .. } = item {
            // Preserve the canonical ID, turn stamp, and all harness metadata.
            *content = vec![ContentItem::InputText {
                text: fragment.render(),
            }];
        }
    }
}

#[cfg(test)]
#[path = "mailbox_inventory_tests.rs"]
mod tests;
