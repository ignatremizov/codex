//! Lossy legacy mirrors cannot clear policy metadata supplied by the canonical item.

use super::ThreadHistoryBuilder;
use super::ThreadItem;

impl ThreadHistoryBuilder {
    pub(super) fn upsert_legacy_collab_item(&mut self, mut item: ThreadItem) {
        let existing = self.current_turn.as_ref().and_then(|turn| {
            turn.items
                .iter()
                .find(|existing| existing.id() == item.id())
        });
        if let (
            Some(ThreadItem::CollabAgentToolCall {
                tool: previous_tool,
                sender_thread_id: previous_sender,
                observe_commentary: previous_commentary,
                wake_on_completion: previous_wake,
                target_messages: previous_target_messages,
                queue_input: previous_queue_input,
                ..
            }),
            ThreadItem::CollabAgentToolCall {
                tool,
                sender_thread_id,
                observe_commentary,
                wake_on_completion,
                target_messages,
                queue_input,
                ..
            },
        ) = (existing, &mut item)
            && *tool == *previous_tool
            && sender_thread_id.as_str() == previous_sender.as_str()
        {
            if observe_commentary.is_none() {
                *observe_commentary = *previous_commentary;
            }
            if wake_on_completion.is_none() {
                *wake_on_completion = *previous_wake;
            }
            if target_messages.is_none() {
                *target_messages = *previous_target_messages;
            }
            if queue_input.is_none() {
                *queue_input = *previous_queue_input;
            }
        }
        // This path is used only by legacy reducers. A later canonical lifecycle item
        // still replaces the complete policy, including an explicit absence of metadata.
        self.upsert_item_in_current_turn(item);
    }
}

#[cfg(test)]
#[path = "collab_legacy_tests.rs"]
mod tests;
