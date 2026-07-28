use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_transcript_parts;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

use super::super::LocalThreadStore;
use super::super::rollout_lineage::RolloutLineage;
use super::segment_paging::page_item_rows;
use super::segment_paging::page_turn_rows;
use super::segment_paging::validate_page_size;
use super::sqlite_integer;
use super::turn_lookup::find_source_turn;
use super::turn_lookup::find_visible_turn;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListTurnsParams;
use crate::StoredThreadItem;
use crate::StoredTurn;
use crate::StoredTurnError;
use crate::StoredTurnItemsView;
use crate::StoredTurnStatus;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;

pub(super) use super::read_freshness::prepare as validate_thread_for_paginated_reads;

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum CursorScope {
    Turns,
    ItemsByCreatedAtOrdinal,
    ItemsByUpdatedAtOrdinal,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HistoryCursor {
    pub requested_thread_id: ThreadId,
    pub rollout_ordinal: u64,
    pub include_anchor: bool,
    pub scope: CursorScope,
}

#[derive(Clone, Copy)]
pub(super) struct RolloutHistoryPosition {
    pub rollout_ordinal: i64,
}

pub(super) struct StoredTurnRow {
    pub position: RolloutHistoryPosition,
    pub turn_id: String,
    pub status: StoredTurnStatus,
    pub error: Option<StoredTurnError>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub first_user_item_id: Option<String>,
    pub final_agent_item_id: Option<String>,
    pub summary_items: Vec<StoredThreadItem>,
}

#[derive(sqlx::FromRow)]
pub(super) struct StoredSummaryColumns {
    summary_first_user_turn_id: Option<String>,
    summary_first_user_item_id: Option<String>,
    summary_first_user_rollout_ordinal: Option<i64>,
    summary_first_user_updated_at_ordinal: Option<i64>,
    summary_first_user_created_at_ms: Option<i64>,
    summary_first_user_item_json: Option<String>,
    summary_final_agent_turn_id: Option<String>,
    summary_final_agent_item_id: Option<String>,
    summary_final_agent_rollout_ordinal: Option<i64>,
    summary_final_agent_updated_at_ordinal: Option<i64>,
    summary_final_agent_created_at_ms: Option<i64>,
    summary_final_agent_item_json: Option<String>,
}

struct StoredSummaryItemColumns {
    turn_id: Option<String>,
    item_id: Option<String>,
    rollout_ordinal: Option<i64>,
    updated_at_ordinal: Option<i64>,
    created_at_ms: Option<i64>,
    item_json: Option<String>,
}

pub(super) struct StoredThreadItemRow {
    pub position: RolloutHistoryPosition,
    pub item: StoredThreadItem,
}

pub(in crate::local) async fn list_turns(
    store: &LocalThreadStore,
    params: ListTurnsParams,
) -> ThreadStoreResult<TurnPage> {
    let prepared = validate_thread_for_paginated_reads(
        store,
        params.thread_id,
        params.include_archived,
        "list_turns",
    )
    .await?;
    validate_page_size(params.page_size)?;
    let lineage = &prepared.lineage;
    let pool = store.thread_history_db().await?;
    let page = page_turn_rows(
        pool,
        params.thread_id,
        lineage,
        params.cursor.as_deref(),
        params.page_size,
        params.sort_direction,
        params.items_view,
    )
    .await?;
    let mut turns = Vec::with_capacity(page.rows.len());
    for turn in page.rows {
        let items = match params.items_view {
            StoredTurnItemsView::NotLoaded => Vec::new(),
            StoredTurnItemsView::Summary => load_summary_items(pool, lineage, &turn).await?,
        };
        turns.push(StoredTurn {
            turn_id: turn.turn_id,
            items,
            items_view: params.items_view,
            status: turn.status,
            error: turn.error,
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
        });
    }

    Ok(TurnPage {
        turns,
        next_cursor: page.next_cursor,
        backwards_cursor: page.backwards_cursor,
    })
}

pub(in crate::local) async fn list_items(
    store: &LocalThreadStore,
    params: ListItemsParams,
) -> ThreadStoreResult<ItemPage> {
    let prepared = validate_thread_for_paginated_reads(
        store,
        params.thread_id,
        params.include_archived,
        "list_items",
    )
    .await?;
    validate_page_size(params.page_size)?;
    let lineage = &prepared.lineage;
    let pool = store.thread_history_db().await?;
    let page = page_item_rows(pool, lineage, &params).await?;

    Ok(ItemPage {
        items: page.rows.into_iter().map(|row| row.item).collect(),
        next_cursor: page.next_cursor,
        backwards_cursor: page.backwards_cursor,
    })
}

async fn load_summary_items(
    pool: &sqlx::SqlitePool,
    lineage: &RolloutLineage,
    turn: &StoredTurnRow,
) -> ThreadStoreResult<Vec<StoredThreadItem>> {
    let source = if matches!(turn.status, StoredTurnStatus::Interrupted)
        && turn.first_user_item_id.is_none()
        && turn.final_agent_item_id.is_none()
    {
        // Synthetic fork-boundary rows inherit the earliest visible source turn.
        find_source_turn(pool, lineage, turn.turn_id.as_str()).await?
    } else {
        find_visible_turn(pool, lineage, turn.turn_id.as_str()).await?
    };
    let mut summary = Vec::new();
    // A logical turn can have inherited rows and later completion arrivals in the child.
    // Query only this turn in each frozen source range; never extend an inherited cutoff.
    for segment in lineage.segments() {
        let start_ordinal = sqlite_integer(segment.start_ordinal(), "rollout ordinal")?;
        let end_ordinal = segment
            .end_ordinal()
            .map(|ordinal| sqlite_integer(ordinal, "rollout ordinal"))
            .transpose()?;
        let rows = sqlx::query(
            r#"
SELECT turn_id, item_id, updated_at_ordinal, created_at_ms, item_json
FROM thread_items
WHERE thread_id = ?
  AND turn_id = ?
  AND rollout_ordinal >= ?
  AND (? IS NULL OR rollout_ordinal < ?)
  AND (
    item_id = ?
    OR item_id = ?
    OR item_id GLOB 'msg_[cesn]_*'
    OR (
      json_extract(item_json, '$.type') = 'collabAgentToolCall'
      AND json_extract(item_json, '$.tool') = 'wait'
      AND json_extract(item_json, '$.status') != 'inProgress'
    )
  )
ORDER BY rollout_ordinal ASC
        "#,
        )
        .bind(segment.rollout_id().to_string())
        .bind(turn.turn_id.as_str())
        .bind(start_ordinal)
        .bind(end_ordinal)
        .bind(end_ordinal)
        .bind(&source.first_user_item_id)
        .bind(&source.final_agent_item_id)
        .fetch_all(pool)
        .await
        .map_err(super::thread_history_error)?;
        summary.extend(
            rows.into_iter()
                .map(stored_thread_item)
                .filter_map(|result| match result {
                    Ok(item)
                        if source.first_user_item_id.as_ref() == Some(&item.item_id)
                            || source.final_agent_item_id.as_ref() == Some(&item.item_id)
                            || turn
                                .summary_items
                                .iter()
                                .any(|ordinary| ordinary.item_id == item.item_id)
                            || is_completion_summary_item(&item) =>
                    {
                        Some(Ok(item))
                    }
                    Ok(_) => None,
                    Err(err) => Some(Err(err)),
                })
                .collect::<ThreadStoreResult<Vec<_>>>()?,
        );
    }
    // A newer segment can carry a later version of an existing canonical item. Preserve its
    // position and identity, rather than deduplicating unrelated messages by their text.
    let mut seen = std::collections::HashSet::new();
    summary.reverse();
    summary.retain(|item| seen.insert(item.item_id.clone()));
    summary.reverse();
    Ok(summary)
}

fn is_completion_summary_item(item: &StoredThreadItem) -> bool {
    use codex_app_server_protocol::CollabAgentTool;
    use codex_app_server_protocol::CollabAgentToolCallStatus;
    use codex_app_server_protocol::ThreadItem;

    // This is display selection over the trusted canonical-to-public projection, not evidence
    // of canonical commit or completion ownership. Private rollout metadata owns those decisions.
    match serde_json::from_slice::<ThreadItem>(&item.item_json) {
        Ok(ThreadItem::AgentMessage {
            id,
            text,
            phase,
            inter_agent_source,
            ..
        }) => {
            id == item.item_id
                && phase == Some(MessagePhase::Commentary)
                && inter_agent_source.is_none()
                && sub_agent_completion_status_from_response_item_id(&id).is_some()
                && sub_agent_completion_transcript_parts(&text)
                    .is_some_and(|(reference, _)| !reference.is_empty())
        }
        Ok(ThreadItem::CollabAgentToolCall { tool, status, .. }) => {
            // Ordinary terminal waits remain useful summary rows too. Only explicitly owned
            // waits survive exact rollback; that masking happened before projection.
            tool == CollabAgentTool::Wait
                && matches!(
                    status,
                    CollabAgentToolCallStatus::Completed | CollabAgentToolCallStatus::Failed
                )
        }
        _ => false,
    }
}

pub(super) fn parse_cursor(
    cursor: Option<&str>,
    requested_thread_id: ThreadId,
    scope: CursorScope,
) -> ThreadStoreResult<Option<HistoryCursor>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let cursor_value: HistoryCursor =
        serde_json::from_str(cursor).map_err(|_| invalid_cursor(cursor))?;
    if cursor_value.requested_thread_id != requested_thread_id || cursor_value.scope != scope {
        return Err(invalid_cursor(cursor));
    }
    Ok(Some(cursor_value))
}

pub(super) fn serialize_cursor(
    requested_thread_id: ThreadId,
    scope: CursorScope,
    rollout_ordinal: i64,
    include_anchor: bool,
) -> ThreadStoreResult<String> {
    let rollout_ordinal =
        u64::try_from(rollout_ordinal).map_err(|_| invalid_cursor("negative rollout ordinal"))?;
    serde_json::to_string(&HistoryCursor {
        requested_thread_id,
        rollout_ordinal,
        include_anchor,
        scope,
    })
    .map_err(super::thread_history_error)
}

pub(super) fn stored_turn_row(row: sqlx::sqlite::SqliteRow) -> ThreadStoreResult<StoredTurnRow> {
    let status = match row.try_get::<String, _>("status")?.as_str() {
        "completed" => StoredTurnStatus::Completed,
        "interrupted" => StoredTurnStatus::Interrupted,
        "failed" => StoredTurnStatus::Failed,
        "inProgress" => StoredTurnStatus::InProgress,
        status => {
            return Err(ThreadStoreError::Internal {
                message: format!("unknown stored turn status: {status}"),
            });
        }
    };
    let error_json = row.try_get::<Option<String>, _>("error_json")?;
    let error = error_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(super::thread_history_error)?;
    Ok(StoredTurnRow {
        position: RolloutHistoryPosition {
            rollout_ordinal: row.try_get("rollout_ordinal")?,
        },
        turn_id: row.try_get("turn_id")?,
        status,
        error,
        started_at: row.try_get("started_at")?,
        completed_at: row.try_get("completed_at")?,
        duration_ms: row.try_get("duration_ms")?,
        first_user_item_id: row.try_get("first_user_item_id")?,
        final_agent_item_id: row.try_get("final_agent_item_id")?,
        summary_items: Vec::new(),
    })
}

impl StoredSummaryColumns {
    pub(super) fn into_stored_items(self) -> ThreadStoreResult<Vec<StoredThreadItem>> {
        let mut summary_items = [
            StoredSummaryItemColumns {
                turn_id: self.summary_first_user_turn_id,
                item_id: self.summary_first_user_item_id,
                rollout_ordinal: self.summary_first_user_rollout_ordinal,
                updated_at_ordinal: self.summary_first_user_updated_at_ordinal,
                created_at_ms: self.summary_first_user_created_at_ms,
                item_json: self.summary_first_user_item_json,
            }
            .into_stored_item()?,
            StoredSummaryItemColumns {
                turn_id: self.summary_final_agent_turn_id,
                item_id: self.summary_final_agent_item_id,
                rollout_ordinal: self.summary_final_agent_rollout_ordinal,
                updated_at_ordinal: self.summary_final_agent_updated_at_ordinal,
                created_at_ms: self.summary_final_agent_created_at_ms,
                item_json: self.summary_final_agent_item_json,
            }
            .into_stored_item()?,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        summary_items.sort_by_key(|(rollout_ordinal, _)| *rollout_ordinal);
        Ok(summary_items.into_iter().map(|(_, item)| item).collect())
    }
}

impl StoredSummaryItemColumns {
    fn into_stored_item(self) -> ThreadStoreResult<Option<(i64, StoredThreadItem)>> {
        let Some(item_id) = self.item_id else {
            return Ok(None);
        };
        let (
            Some(turn_id),
            Some(rollout_ordinal),
            Some(updated_at_ordinal),
            Some(created_at_ms),
            Some(item_json),
        ) = (
            self.turn_id,
            self.rollout_ordinal,
            self.updated_at_ordinal,
            self.created_at_ms,
            self.item_json,
        )
        else {
            return Err(ThreadStoreError::Internal {
                message: "stored summary item is missing joined columns".to_string(),
            });
        };
        Ok(Some((
            rollout_ordinal,
            StoredThreadItem {
                turn_id,
                item_id,
                updated_at_ordinal: stored_updated_at_ordinal(updated_at_ordinal)?,
                created_at_ms,
                item_json: item_json.into_bytes(),
            },
        )))
    }
}

pub(super) fn stored_thread_item_row(
    row: sqlx::sqlite::SqliteRow,
) -> ThreadStoreResult<StoredThreadItemRow> {
    let rollout_ordinal = row.try_get::<i64, _>("rollout_ordinal")?;
    if rollout_ordinal < 0 {
        return Err(ThreadStoreError::Internal {
            message: format!("invalid stored item rollout ordinal: {rollout_ordinal}"),
        });
    }
    Ok(StoredThreadItemRow {
        position: RolloutHistoryPosition { rollout_ordinal },
        item: stored_thread_item(row)?,
    })
}

fn stored_thread_item(row: sqlx::sqlite::SqliteRow) -> ThreadStoreResult<StoredThreadItem> {
    let updated_at_ordinal = stored_updated_at_ordinal(row.try_get("updated_at_ordinal")?)?;
    Ok(StoredThreadItem {
        turn_id: row.try_get("turn_id")?,
        item_id: row.try_get("item_id")?,
        updated_at_ordinal,
        created_at_ms: row.try_get("created_at_ms")?,
        item_json: row.try_get::<String, _>("item_json")?.into_bytes(),
    })
}

fn stored_updated_at_ordinal(updated_at_ordinal: i64) -> ThreadStoreResult<u64> {
    u64::try_from(updated_at_ordinal).map_err(|_| ThreadStoreError::Internal {
        message: format!("invalid stored item updated-at ordinal: {updated_at_ordinal}"),
    })
}

pub(super) fn invalid_cursor(cursor: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    }
}
