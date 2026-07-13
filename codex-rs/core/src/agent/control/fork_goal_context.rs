//! Remove runtime-owned goal steering without treating quoted text as authority.

use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) fn retain_without_goal_context(envelope: &mut ResponseItemEnvelope) -> bool {
    if envelope
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.client_authored)
    {
        return true;
    }
    let ResponseItem::Message {
        role,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return true;
    };
    if role != "user"
        || !internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.0 == "goal.internal_context"))
    {
        return true;
    }
    let Some(mut content) = to_annotated_content(&mut envelope.item) else {
        return true;
    };
    content.retain(|fragment| {
        if fragment.kind().0 != "goal.internal_context" {
            return true;
        }
        let ContentItem::InputText { text } = fragment.content() else {
            return true;
        };
        let text = text.trim();
        !(text.starts_with("<codex_internal_context source=\"goal\">")
            && text.ends_with("</codex_internal_context>"))
    });
    !content.is_empty() && set_annotated_content(&mut envelope.item, content).is_some()
}

#[cfg(test)]
#[path = "fork_goal_context_tests.rs"]
mod tests;
