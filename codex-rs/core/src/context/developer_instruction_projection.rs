//! Project effective session instructions without rewriting historical audit records.

use super::ContextualUserFragment;
use super::DeveloperInstructions;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

/// A resumed or history-forked session can have a new resolved instruction value while
/// retaining an older initial-context bundle. Replace only harness-classified configuration
/// fragments; managed policy, client messages and unclassified legacy text remain intact.
pub(crate) fn project_developer_instructions(
    input: &mut Vec<ResponseItem>,
    instructions: Option<&str>,
) {
    let Some(instructions) = instructions else {
        // No resolved override: keep the historic instructions, including legacy context.
        return;
    };
    let replacement = (!instructions.is_empty()).then_some(instructions);
    let kind = DeveloperInstructions::new("").content_kind();
    let mut inserted = false;
    input.retain_mut(|item| {
        let ResponseItem::Message {
            role,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return true;
        };
        if role != "developer"
            || !metadata
                .content_item_kinds
                .as_ref()
                .is_some_and(|kinds| kinds.contains(&kind))
        {
            return true;
        }
        let Some(mut content) = to_annotated_content(item) else {
            return true;
        };
        content.retain_mut(|fragment| {
            if fragment.kind() != &kind {
                return true;
            }
            let Some(instructions) = replacement.filter(|_| !inserted) else {
                return false;
            };
            *fragment.content_mut() = ContentItem::InputText {
                text: instructions.to_owned(),
            };
            inserted = true;
            true
        });
        if content.is_empty() {
            return false;
        }
        let _ = set_annotated_content(item, content);
        true
    });
    if !inserted && let Some(instructions) = replacement {
        input.insert(
            /*index*/ 0,
            ContextualUserFragment::into(DeveloperInstructions::new(instructions)),
        );
    }
}

#[cfg(test)]
#[path = "developer_instruction_projection_tests.rs"]
mod tests;
