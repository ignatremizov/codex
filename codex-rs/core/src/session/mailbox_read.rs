//! Check-mail presentation shares the owned result/consumption publication boundary.

use super::*;
use codex_protocol::items::MailboxReadItem;
use codex_protocol::items::MailboxReadSelector;
use codex_thread_store::MailboxClaim;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxSelection;
use codex_thread_store::MailboxSender;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreResult;

impl Session {
    /// Caller retains the canonical barrier. Do not use the ordinary retrying event queue.
    pub(super) async fn publish_mailbox_read(
        &self,
        turn_id: &str,
        call_id: &str,
        claim: &MailboxClaim,
    ) -> ThreadStoreResult<()> {
        let selector = match &claim.selection {
            MailboxSelection::All => MailboxReadSelector::All,
            MailboxSelection::Senders(senders) => match senders.as_slice() {
                [MailboxSender::User] => MailboxReadSelector::User,
                [MailboxSender::Agent(thread_id)] => MailboxReadSelector::Agent {
                    thread_id: *thread_id,
                },
                _ => {
                    return Err(ThreadStoreError::Conflict {
                        message: "check_mail claim has a non-canonical selection".into(),
                    });
                }
            },
        };
        let read = MailboxReadItem {
            id: call_id.to_owned(),
            selector,
            consumed_count: claim
                .messages
                .iter()
                .filter(|member| member.message.state == MailboxMessageState::Consumed)
                .count() as u64,
            rejected_count: claim
                .messages
                .iter()
                .filter(|member| member.message.state == MailboxMessageState::Rejected)
                .count() as u64,
        };
        let history = self
            .services
            .thread_store
            .load_mailbox_canonical_history(self.thread_id)
            .await?;
        let mut recorded = false;
        for item in &history {
            match item {
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.turn_id == turn_id && event.item.id() == call_id =>
                {
                    if event.thread_id != self.thread_id
                        || event.item != TurnItem::MailboxRead(read.clone())
                    {
                        return Err(ThreadStoreError::Conflict {
                            message: "conflicting mailbox read presentation".into(),
                        });
                    }
                    recorded = true;
                }
                RolloutItem::EventMsg(EventMsg::ItemStarted(event))
                    if event.turn_id == turn_id && event.item.id() == call_id =>
                {
                    return Err(ThreadStoreError::Conflict {
                        message: "mailbox read identity already started another item".into(),
                    });
                }
                RolloutItem::ResponseItem(item)
                    if item.id().is_some_and(|id| id.as_str() == call_id) =>
                {
                    return Err(ThreadStoreError::Conflict {
                        message: "mailbox read identity collides with a response".into(),
                    });
                }
                _ => {}
            }
        }
        if recorded {
            return Ok(());
        }
        let event = Event {
            id: turn_id.to_owned(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_id.to_owned(),
                item: TurnItem::MailboxRead(read),
                started_at_ms: None,
                completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
            }),
        };
        let live = self
            .live_thread()
            .ok_or_else(|| ThreadStoreError::Conflict {
                message: "mailbox read requires canonical persistence".into(),
            })?;
        live.append_completion_items_and_flush_canonical(&[RolloutItem::EventMsg(
            event.msg.clone(),
        )])
        .await?;
        self.tx_event
            .send(event)
            .await
            .map_err(|_| ThreadStoreError::Conflict {
                message: "mailbox read presentation stream closed after canonical commit".into(),
            })
    }
}
