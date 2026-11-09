//! Read the exact receiving turn without creating a session or client subscription.

use super::*;

impl ThreadRequestProcessor {
    pub(crate) async fn queued_input_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Result<Option<Turn>, JSONRPCErrorError> {
        let stored = self
            .thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(thread_store_list_error)?;
        if stored.history_mode != ThreadHistoryMode::Paginated {
            let mut turn = {
                let cursor = serialize_thread_turns_cursor(turn_id, /*include_anchor*/ true)?;
                let items = self
                    .load_thread_turns_list_history(thread_id)
                    .await
                    .map_err(thread_read_view_error)?;
                let page = build_thread_turns_page_response(
                    &items,
                    ThreadStatus::Idle,
                    /*has_live_running_thread*/ true,
                    /*active_turn*/ None,
                    ThreadTurnsPageOptions {
                        cursor: Some(&cursor),
                        limit: Some(1),
                        sort_direction: SortDirection::Desc,
                        items_view: TurnItemsView::Full,
                    },
                )?;
                page.data.into_iter().find(|turn| turn.id == turn_id)
            };
            if let Some(turn) = &mut turn {
                turn.items = std::mem::take(&mut turn.items)
                    .into_iter()
                    .rev()
                    .find(is_direct_final)
                    .into_iter()
                    .collect();
                turn.items_view = TurnItemsView::Summary;
                self.reconcile_queued_turn_owner(thread_id, turn).await?;
            }
            return Ok(turn);
        }
        let mut position = None;
        let stored = loop {
            let page = self
                .thread_store
                .list_turns(StoreListTurnsParams {
                    thread_id,
                    include_archived: true,
                    cursor: position,
                    page_size: 100,
                    sort_direction: StoreSortDirection::Desc,
                    items_view: StoredTurnItemsView::NotLoaded,
                })
                .await
                .map_err(thread_store_list_error)?;
            if let Some(turn) = page.turns.into_iter().find(|turn| turn.turn_id == turn_id) {
                break turn;
            }
            position = page.next_cursor;
            if position.is_none() {
                return Ok(None);
            }
        };
        let mut turn = stored_turn_to_api_turn(stored, TurnItemsView::Summary)?;
        self.reconcile_queued_turn_owner(thread_id, &mut turn)
            .await?;
        if turn.status == TurnStatus::InProgress {
            return Ok(Some(turn));
        }
        let mut cursor = None;
        loop {
            let page = self
                .thread_items_list_response_inner(ThreadItemsListParams {
                    thread_id: thread_id.to_string(),
                    turn_id: Some(turn_id.to_string()),
                    cursor,
                    limit: Some(1),
                    sort_direction: Some(SortDirection::Desc),
                })
                .await?;
            if let Some(item) = page
                .data
                .into_iter()
                .map(|entry| entry.item)
                .find(is_direct_final)
            {
                turn.items.push(item);
                break;
            }
            cursor = page.next_cursor.map(ThreadItemsListCursor::Opaque);
            if cursor.is_none() {
                break;
            }
        }
        Ok(Some(turn))
    }

    /// Local inventory cannot tell whether a different profile owns this writer.
    /// Successful reservation proves an unfinished stored turn has lost its owner;
    /// a conflicting live writer leaves its canonical status authoritative.
    async fn reconcile_queued_turn_owner(
        &self,
        thread_id: ThreadId,
        turn: &mut Turn,
    ) -> Result<(), JSONRPCErrorError> {
        if turn.status != TurnStatus::InProgress {
            return Ok(());
        }
        // Core acknowledges input before task registration. Its previous agent
        // status can still be Interrupted while this new turn is starting.
        if self.thread_manager.get_thread(thread_id).await.is_ok() {
            return Ok(());
        }
        match self
            .thread_store
            .reserve_thread_writers(vec![thread_id])
            .await
        {
            Ok(_reservation) => {
                turn.status = TurnStatus::Interrupted;
                Ok(())
            }
            Err(ThreadStoreError::Conflict { .. }) => Ok(()),
            Err(error) => Err(thread_store_list_error(error)),
        }
    }
}

fn is_direct_final(item: &ThreadItem) -> bool {
    matches!(&item.payload, ThreadItemPayload::AgentMessage {
        id,
        phase: None | Some(codex_protocol::models::MessagePhase::FinalAnswer),
        inter_agent_source: None,
        attribution: None,
        ..
    } if codex_protocol::protocol::agent_delivery_receipt_from_response_item_id(id).is_none()
        && codex_protocol::protocol::sub_agent_completion_status_from_response_item_id(id).is_none())
}
