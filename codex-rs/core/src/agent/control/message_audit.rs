//! Live root transcript copies of accepted V1 peer input, without root model or rollout delivery.

use super::*;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::agent_delivery_receipt_item;
use codex_protocol::protocol::attributed_agent_message_transcript_parts;

impl LocalAgentControl {
    /// Acknowledgement of Main's output at a child is UI-only, not another child's response.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "live receipt placement must be atomic with root turn transitions"
    )]
    pub(crate) async fn mirror_agent_delivery_receipt(
        &self,
        sender: ThreadId,
        recipient: ThreadId,
        phase: MessagePhase,
        recipient_model_visibility: SubAgentCompletionModelVisibility,
        delivery_id: &str,
        text: &str,
    ) -> CodexResult<()> {
        // Presentation-only delivery belongs to the recipient's durable transcript.
        // Do not add another root row acknowledging that hidden delivery.
        if recipient_model_visibility == SubAgentCompletionModelVisibility::NotVisible {
            return Ok(());
        }
        let Some(root_id) = self.bound_session_id().map(ThreadId::from) else {
            return Ok(());
        };
        if sender != root_id || recipient == root_id {
            return Ok(());
        }
        let Some(receipt) = agent_delivery_receipt_item(
            sender,
            recipient,
            phase,
            recipient_model_visibility,
            delivery_id,
            text,
        ) else {
            return Ok(());
        };
        let state = self.upgrade()?;
        let root = state.get_thread(root_id).await?;
        if !Arc::ptr_eq(
            &root.session.services.agent_control.wait_agent_presentations,
            &self.wait_agent_presentations,
        ) {
            return Ok(());
        }
        let active_turn = root.session.active_turn.lock().await;
        let turn_id = active_turn
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .map(|task| task.turn_context.sub_id.clone())
            .or_else(|| root.session.active_agent_response_turn_id())
            .unwrap_or_else(|| receipt.id.clone());
        let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
        // Deliberately bypass send_event_raw: even non-persisting session events notify response
        // observers. The recipient already owns the durable record and any model delivery.
        root.session
            .deliver_agent_audit_event(Event {
                id: receipt.id.clone(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: root_id,
                    turn_id,
                    item: TurnItem::AgentMessage(receipt),
                    started_at_ms: Some(completed_at_ms),
                    completed_at_ms,
                }),
            })
            .await;
        drop(active_turn);
        Ok(())
    }

    /// Mirror input when the recipient records it, including queued input only once admitted.
    /// Mailbox acceptance notices instead carry their reserved ID and immutable accepted payload;
    /// they acknowledge storage only, before any receiver consumption.
    /// Root-directed input already has its own presentation and must not be copied again.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "live audit placement must be atomic with root turn transitions"
    )]
    pub(crate) async fn mirror_attributed_agent_input(
        &self,
        recipient: ThreadId,
        item: &TurnItem,
    ) -> CodexResult<()> {
        let TurnItem::AgentMessage(item) = item else {
            return Ok(());
        };
        if !item.is_attributed_agent_input_presentation()
            && !codex_protocol::is_mailbox_acceptance_receipt_id(&item.id)
        {
            return Ok(());
        }
        let (sender, legacy_message) = if let Some(attribution) = &item.attribution {
            if attribution.recipient.thread_id != recipient {
                return Ok(());
            }
            // Array-target sends are represented once by their sender's live batch lifecycle.
            // Their recipient-owned records retain this correlation for replay and mailbox
            // recovery, but must not create one root row per recipient.
            if attribution.batch_id.is_some() {
                return Ok(());
            }
            (attribution.sender.thread_id, None)
        } else {
            let [AgentMessageContent::Text { text }] = item.content.as_slice() else {
                return Ok(());
            };
            let Some((sender, message)) = attributed_agent_message_transcript_parts(text) else {
                return Ok(());
            };
            let Ok(sender) = ThreadId::from_string(sender) else {
                return Ok(());
            };
            (sender, Some(message))
        };
        let Some(root) = self.bound_session_id().map(ThreadId::from) else {
            return Ok(());
        };
        if root == sender || root == recipient {
            return Ok(());
        }
        let state = self.upgrade()?;
        // The recipient owns the durable input. Main receives only a live presentation: no
        // rollout append, cold resume, observer installation, or model turn for this copy.
        let root = state.get_thread(root).await?;
        if !Arc::ptr_eq(
            &root.session.services.agent_control.wait_agent_presentations,
            &self.wait_agent_presentations,
        ) {
            return Ok(());
        }
        let mut audit = item.clone();
        if let Some(message) = legacy_message {
            audit.content = vec![AgentMessageContent::Text {
                text: format!("Agent message from `{sender}` to `{recipient}`:\n\n{message}"),
            }];
        }
        audit.phase = Some(MessagePhase::Commentary);
        let active_turn = root.session.active_turn.lock().await;
        let turn_id = active_turn
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .map(|task| task.turn_context.sub_id.clone())
            .or_else(|| root.session.active_agent_response_turn_id())
            .unwrap_or_else(|| audit.id.clone());
        let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
        root.session
            .deliver_agent_audit_event(Event {
                id: audit.id.clone(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: root.session.thread_id(),
                    turn_id,
                    item: TurnItem::AgentMessage(audit),
                    started_at_ms: Some(completed_at_ms),
                    completed_at_ms,
                }),
            })
            .await;
        drop(active_turn);
        Ok(())
    }

    /// Mirror one completed array-target lifecycle to the root as a live-only presentation.
    ///
    /// The sender's canonical lifecycle remains the only durable source. Recipient input records
    /// retain their individual trusted attribution and outcomes; this copy only lets the root
    /// render the existing consolidated batch cell without persisting or delivering another
    /// input.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "live batch audit placement must be atomic with root turn transitions"
    )]
    pub(crate) async fn mirror_agent_input_batch(
        &self,
        mut item: CollabAgentToolCallItem,
    ) -> CodexResult<()> {
        if item.tool != CollabAgentTool::SendInput || item.input_batch.is_none() {
            return Ok(());
        }
        let Some(root_id) = self.bound_session_id().map(ThreadId::from) else {
            return Ok(());
        };
        if item.sender_thread_id == root_id {
            return Ok(());
        }
        let state = self.upgrade()?;
        let root = state.get_thread(root_id).await?;
        if !Arc::ptr_eq(
            &root.session.services.agent_control.wait_agent_presentations,
            &self.wait_agent_presentations,
        ) {
            return Ok(());
        }
        // Tool-call IDs are scoped to the sender's history. The root copy needs a distinct
        // identity even when another child or the root used the same model-authored call ID.
        item.id = format!("agent-input-batch/{}/{}", item.sender_thread_id, item.id);
        let active_turn = root.session.active_turn.lock().await;
        let turn_id = active_turn
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .map(|task| task.turn_context.sub_id.clone())
            .or_else(|| root.session.active_agent_response_turn_id())
            .unwrap_or_else(|| item.id.clone());
        let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
        root.session
            .deliver_agent_audit_event(Event {
                id: item.id.clone(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: root_id,
                    turn_id,
                    item: TurnItem::CollabAgentToolCall(item),
                    started_at_ms: Some(completed_at_ms),
                    completed_at_ms,
                }),
            })
            .await;
        drop(active_turn);
        Ok(())
    }
}
