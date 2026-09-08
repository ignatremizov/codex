//! Bounded inventory guidance, never user-authored input or mailbox payload.
//!
//! Thread-store owns the deterministic durable format and legacy recognition.
//! This typed fragment keeps the model-context classification at its core boundary.

use super::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_thread_store::MailboxInventoryNotification;
use codex_thread_store::ThreadStoreError;

pub(crate) struct MailboxInventoryContext {
    id: ResponseItemId,
    turn_id: String,
    text: String,
}

impl TryFrom<&MailboxInventoryNotification> for MailboxInventoryContext {
    type Error = ThreadStoreError;

    fn try_from(notification: &MailboxInventoryNotification) -> Result<Self, Self::Error> {
        let context = notification.context()?;
        let ResponseItem::Message {
            id: Some(id),
            content,
            ..
        } = context.item
        else {
            return Err(ThreadStoreError::Internal {
                message: "validated mailbox inventory has no message identity".to_string(),
            });
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return Err(ThreadStoreError::Internal {
                message: "validated mailbox inventory has an unexpected content shape".to_string(),
            });
        };
        Ok(Self {
            id,
            turn_id: notification.id.clone(),
            text: text.clone(),
        })
    }
}

impl ContextualUserFragment for MailboxInventoryContext {
    fn role(&self) -> &'static str {
        "developer"
    }
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("mailbox.inventory".to_string())
    }
    fn markers(&self) -> (&'static str, &'static str) {
        ("", "")
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
    fn body(&self) -> String {
        self.text.clone()
    }
}

impl From<MailboxInventoryContext> for ResponseItemEnvelope {
    fn from(fragment: MailboxInventoryContext) -> Self {
        let id = fragment.id.clone();
        let turn_id = fragment.turn_id.clone();
        let mut item = ContextualUserFragment::into(fragment);
        item.set_id(Some(id));
        item.set_turn_id_if_missing(&turn_id);
        Self::new(item)
    }
}
