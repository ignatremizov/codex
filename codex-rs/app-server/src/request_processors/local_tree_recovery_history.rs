//! Inspect durable unfinished work before allocating a recovered agent runtime.

use super::ThreadRequestProcessor;
use super::select_unfinished_own_turn;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnContextItem;
use codex_rollout::RolloutItem;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use std::io;

pub(super) struct RecoveryHistory {
    pub(super) context: TurnContextItem,
    pub(super) own_segment: Vec<RolloutItem>,
}

impl ThreadRequestProcessor {
    pub(super) async fn local_recovery_history(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<Option<RecoveryHistory>> {
        let mut segments = self
            .thread_store
            .load_canonical_artifact_segments(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .map_err(io::Error::other)?;
        let Some(own_segment) = segments.segments.pop() else {
            return Ok(None);
        };
        let Some(context) = select_unfinished_own_turn(thread_id, &own_segment)? else {
            return Ok(None);
        };
        if own_segment.iter().any(|item| matches!(item,
            RolloutItem::SessionMeta(meta) if meta.meta.history_mode == ThreadHistoryMode::Paginated))
        {
            let page = self
                .thread_store
                .list_turns(ListTurnsParams {
                    thread_id,
                    include_archived: false,
                    cursor: None,
                    page_size: 1,
                    sort_direction: SortDirection::Desc,
                    items_view: StoredTurnItemsView::NotLoaded,
                })
                .await
                .map_err(io::Error::other)?;
            if !page.turns.first().is_some_and(|turn| {
                Some(&turn.turn_id) == context.turn_id.as_ref()
                    && turn.status == StoredTurnStatus::InProgress
            }) {
                return Ok(None);
            }
        }
        let model_context = self
            .thread_store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .map_err(io::Error::other)?;
        if model_context.thread_id != thread_id
            || !model_context.items.iter().any(|item| {
                matches!(item,
                RolloutItem::TurnContext(saved) if saved == &context)
            })
        {
            return Err(io::Error::other(
                "unfinished turn has no recoverable model context",
            ));
        }
        Ok(Some(RecoveryHistory {
            context,
            own_segment,
        }))
    }
}
