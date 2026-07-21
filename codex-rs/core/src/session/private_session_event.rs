//! Parent observation shared by direct and independently owned event publication.

use async_channel::Sender;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PrivateSessionEvent;
use codex_protocol::protocol::PrivateSessionEventPayload;

/// Forward committed reviewer events before the private consumer can finish the review.
pub(super) async fn forward_to_parent(parent: &Sender<Event>, thread_id: ThreadId, event: &Event) {
    let payload = match &event.msg {
        EventMsg::RawResponseItem(item) => PrivateSessionEventPayload::RecordedItem(item.clone()),
        EventMsg::TurnAborted(aborted) => PrivateSessionEventPayload::TurnAborted(aborted.clone()),
        _ => return,
    };
    // Observer delivery bypasses the parent's history and retains the reviewer's identity.
    let _ = parent
        .send(Event {
            id: event.id.clone(),
            msg: EventMsg::PrivateSessionEvent(PrivateSessionEvent {
                thread_id,
                event: payload,
            }),
        })
        .await;
}
