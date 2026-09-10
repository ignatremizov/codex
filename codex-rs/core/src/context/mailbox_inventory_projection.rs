//! Disposable inventory presentation. Canonical snapshots remain recovery authority.

use super::ContextualUserFragment;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_thread_store::MailboxInventoryContextView;
use codex_thread_store::MailboxSender;
use codex_utils_string::approx_bytes_for_tokens;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

const INVENTORY_TOKENS: usize = 768;
const HEADING: &str = "Pending mail snapshot (not consumed):\n";
const GUIDANCE: &str = "\ncheck_mail: {\"from\":\"<ref>\"} or {} for all.";
const OMITTED: &str =
    " Additional pending senders are omitted; check_mail with no filter includes their mail.";

#[derive(Serialize)]
struct PendingSender {
    from: String,
    count: u64,
}

struct ProjectedMailboxInventoryContext {
    receiver: String,
    pending_senders: Vec<PendingSender>,
    omitted: bool,
}

impl ContextualUserFragment for ProjectedMailboxInventoryContext {
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

/// Projects recognized inventory text using only this request's advertised receiver namespace.
/// A bounded durable context supplies only its original sender prefix, never omitted payloads or
/// proof of the complete snapshot. This path does not read live mail or acknowledge a frontier.
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
        let Some(snapshot) = MailboxInventoryContextView::from_item(item, receiver) else {
            continue;
        };
        let mut fragment = ProjectedMailboxInventoryContext {
            receiver: reference(receiver),
            pending_senders: Vec::new(),
            omitted: false,
        };
        let mut remaining = approx_bytes_for_tokens(INVENTORY_TOKENS)
            .saturating_sub(fragment.render().len() + OMITTED.len());
        for group in snapshot.pending_senders {
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
        fragment.omitted |= snapshot.omitted;
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
