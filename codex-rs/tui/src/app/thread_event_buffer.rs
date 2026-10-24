//! Bounded replay-buffer policy for per-thread app-server events.

use super::ServerNotification;
use super::ThreadBufferedEvent;
use super::ThreadEventStore;
use std::borrow::Cow;
use std::io;

// Keep merged text finite so continued streaming still reaches bounded replay eviction.
const MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES: usize = 4 * 1024;
const MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES: usize = 256 * 1024;
// Bound every notification payload, including command output and completed items.
// Serialized bytes are a payload budget, not an exact measurement of heap allocations.
const MAX_BUFFERED_NOTIFICATION_BYTES: usize = 8 * 1024 * 1024;

/// Count serialized payload bytes without allocating another copy of the payload.
pub(super) fn replay_payload_bytes(value: &impl serde::Serialize) -> usize {
    #[derive(Default)]
    struct ByteCount(usize);

    impl io::Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_BUFFERED_NOTIFICATION_BYTES.saturating_sub(self.0) {
                return Err(io::Error::other("replay notification byte budget exceeded"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut count = ByteCount::default();
    if serde_json::to_writer(&mut count, value).is_err() {
        return MAX_BUFFERED_NOTIFICATION_BYTES + 1;
    }
    count.0
}

impl ThreadEventStore {
    pub(super) fn push_replay_notification(&mut self, notification: Cow<'_, ServerNotification>) {
        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && delta.delta.len() > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
        {
            return;
        }

        let notification_bytes = replay_payload_bytes(notification.as_ref());
        if notification_bytes > MAX_BUFFERED_NOTIFICATION_BYTES {
            // Dropping one oversized payload must not evict the useful replay window.
            // A skipped completed item can no longer certify cached prompt-edit history.
            self.turn_history_complete = false;
            return;
        }

        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && let Some(ThreadBufferedEvent::Notification(previous)) = self.buffer.back_mut()
            && let ServerNotification::AgentMessageDelta(previous) = previous.as_mut()
            && previous.thread_id == delta.thread_id
            && previous.turn_id == delta.turn_id
            && previous.item_id == delta.item_id
            && previous.delta.len().saturating_add(delta.delta.len())
                <= MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES
        {
            let previous_bytes = replay_payload_bytes(&previous.delta);
            previous.delta.push_str(&delta.delta);
            self.buffered_notification_bytes +=
                replay_payload_bytes(&previous.delta).saturating_sub(previous_bytes);
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
            self.evict_overflowing_events();
            return;
        }

        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref() {
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
        }
        self.buffered_notification_bytes += notification_bytes;
        self.buffer
            .push_back(ThreadBufferedEvent::Notification(Box::new(
                notification.into_owned(),
            )));
        self.evict_overflowing_events();
    }

    pub(super) fn push_buffered_event(&mut self, event: ThreadBufferedEvent) {
        if let ThreadBufferedEvent::Notification(notification) = event {
            self.push_replay_notification(Cow::Owned(*notification));
            return;
        }
        self.buffer.push_back(event);
        self.evict_overflowing_events();
    }

    fn evict_overflowing_events(&mut self) {
        while self.buffer.len() > self.capacity
            || self.buffered_agent_message_delta_bytes > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
            || self.buffered_notification_bytes > MAX_BUFFERED_NOTIFICATION_BYTES
        {
            let index = if self.buffer.len() > self.capacity
                || self.buffered_agent_message_delta_bytes > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
            {
                0
            } else {
                // Payload pressure must not discard an unresolved interactive request.
                let Some(index) = self
                    .buffer
                    .iter()
                    .position(|event| matches!(event, ThreadBufferedEvent::Notification(_)))
                else {
                    break;
                };
                index
            };
            let Some(removed) = self.buffer.remove(index) else {
                break;
            };
            if super::thread_turn_materialization::event_changes_materialized_turns(&removed) {
                self.turn_history_complete = false;
            }
            match removed {
                ThreadBufferedEvent::Notification(notification) => {
                    self.buffered_notification_bytes = self
                        .buffered_notification_bytes
                        .saturating_sub(replay_payload_bytes(notification.as_ref()));
                    if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref() {
                        self.buffered_agent_message_delta_bytes = self
                            .buffered_agent_message_delta_bytes
                            .saturating_sub(delta.delta.len());
                    }
                }
                ThreadBufferedEvent::Request(request) => self
                    .pending_interactive_replay
                    .note_evicted_server_request(request.as_ref()),
                ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::Mcp(_)
                | ThreadBufferedEvent::FeedbackSubmission(_) => {}
            }
        }
    }
}

#[cfg(test)]
#[path = "thread_event_buffer_tests.rs"]
mod tests;
