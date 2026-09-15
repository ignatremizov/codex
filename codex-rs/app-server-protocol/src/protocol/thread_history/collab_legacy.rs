//! Lossy legacy mirrors cannot clear policy metadata supplied by the canonical item.

use super::ItemTimes;
use super::ThreadHistoryBuilder;
use super::ThreadItemPayload;

impl ThreadHistoryBuilder {
    pub(super) fn upsert_legacy_collab_item(
        &mut self,
        mut item: ThreadItemPayload,
        times: ItemTimes,
    ) {
        let existing = self.current_turn.as_ref().and_then(|turn| {
            turn.items
                .iter()
                .find(|existing| existing.id() == item.id())
        });
        if let (
            Some(ThreadItemPayload::CollabAgentToolCall {
                tool: previous_tool,
                sender_thread_id: previous_sender,
                observe_commentary: previous_commentary,
                wake_on_completion: previous_wake,
                target_messages: previous_target_messages,
                queue_input: previous_queue_input,
                mailbox_input: previous_mailbox_input,
                input_batch: previous_input_batch,
                ..
            }),
            ThreadItemPayload::CollabAgentToolCall {
                tool,
                sender_thread_id,
                observe_commentary,
                wake_on_completion,
                target_messages,
                queue_input,
                mailbox_input,
                input_batch,
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
            if mailbox_input.is_none() {
                *mailbox_input = *previous_mailbox_input;
            }
            if input_batch.is_none() {
                *input_batch = previous_input_batch.clone();
            }
        }
        // This path is used only by legacy reducers. A later canonical lifecycle item
        // still replaces the complete policy, including an explicit absence of metadata.
        self.upsert_item(/*turn_id*/ None, item, times);
    }
}

#[cfg(test)]
#[path = "collab_legacy_tests.rs"]
mod tests;
