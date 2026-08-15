use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;

use crate::CodexThread;

impl CodexThread {
    /// Persist and publish a user-authored agent control action in the source transcript.
    pub async fn record_user_agent_control(&self, item: UserAgentControlItem) -> CodexResult<()> {
        let item_id = item.id.clone();
        // Keep the active-turn decision and durable append atomic with source turn
        // start/finalization. Otherwise an idle observation can select a standalone synthetic
        // turn, then race a new source TurnStarted event; replay would see the synthetic completed
        // turn after the new active turn and incorrectly finalize that turn.
        let active_turn = self.session.active_turn.lock().await;
        let turn_id = active_turn
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .map(|task| task.turn_context.sub_id.clone())
            .or_else(|| self.session.active_agent_response_turn_id())
            .unwrap_or_else(|| item_id.clone());
        let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
        let result = self
            .session
            .send_event_raw_flushed(Event {
                id: item_id.clone(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: self.session.thread_id(),
                    turn_id,
                    item: TurnItem::UserAgentControl(item),
                    started_at_ms: Some(completed_at_ms),
                    completed_at_ms,
                }),
            })
            .await;
        drop(active_turn);
        result
            .map_err(|err| CodexErr::Fatal(format!("failed to persist agent control item: {err}")))
    }
}
