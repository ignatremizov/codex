//! Live root transcript copies of accepted V1 peer input, without root model or rollout delivery.

use super::*;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::protocol::attributed_agent_message_transcript_parts;

impl AgentControl {
    /// Mirror input when the recipient records it, including queued input only once admitted.
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
        if !item.is_attributed_agent_input_presentation() {
            return Ok(());
        }
        let (sender, legacy_message) = if let Some(attribution) = &item.attribution {
            if attribution.recipient.thread_id != recipient {
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
        let root = state.get_thread_including_pending(root).await?;
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
            .deliver_event_raw(Event {
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
}
