//! Resolve a steer cut against its original, still-visible canonical source record.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;

use codex_app_server_protocol::ThreadItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_rollout::RolloutItem;
use codex_rollout::UserPromptSource;
use sqlx::Row;

use super::super::LocalThreadStore;
use super::super::rollout_lineage::RolloutLineage;
use super::super::thread_history::find_source_turn;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn before_item(
    store: &LocalThreadStore,
    lineage: &RolloutLineage,
    turn_id: &str,
    item_id: &str,
) -> ThreadStoreResult<HistoryPosition> {
    let pool = store.thread_history_db().await?;
    let turn = find_source_turn(pool, lineage, turn_id).await?;
    if turn.first_user_item_id.is_none() || turn.first_user_item_id.as_deref() == Some(item_id) {
        return Err(ThreadStoreError::InvalidRequest {
            message: "beforeItemId must select a steer; omit it to exclude the whole turn".into(),
        });
    }
    let segment = lineage
        .segments()
        .iter()
        .find(|segment| segment.rollout_id() == turn.rollout_id)
        .ok_or_else(|| invalid_boundary(turn_id, item_id))?;
    let row = sqlx::query(
        "SELECT rollout_ordinal, item_json FROM thread_items \
         WHERE thread_id = ? AND turn_id = ? AND item_id = ?",
    )
    .bind(turn.rollout_id.to_string())
    .bind(turn_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("failed to resolve prompt boundary: {error}"),
    })?
    .ok_or_else(|| invalid_boundary(turn_id, item_id))?;
    let ordinal = u64::try_from(row.get::<i64, _>("rollout_ordinal"))
        .map_err(|_| invalid_boundary(turn_id, item_id))?;
    let item: ThreadItem = serde_json::from_str(row.get::<&str, _>("item_json"))
        .map_err(|_| invalid_boundary(turn_id, item_id))?;
    if !matches!(item, ThreadItem::UserMessage { ref id, .. } if id == item_id)
        || ordinal < segment.start_ordinal()
        || segment.end_ordinal().is_some_and(|end| ordinal >= end)
    {
        return Err(invalid_boundary(turn_id, item_id));
    }
    let rollout_id = turn.rollout_id;
    let start_offset = turn
        .rollout_byte_offset
        .and_then(|offset| u64::try_from(offset).ok())
        .ok_or_else(|| invalid_boundary(turn_id, item_id))?;
    let path = segment.rollout_path.clone();
    let end_offset = segment
        .end
        .map(|end| end.end_byte_offset)
        .into_iter()
        .chain(
            turn.rollout_end_byte_offset
                .and_then(|offset| u64::try_from(offset).ok()),
        )
        .min()
        .ok_or_else(|| invalid_boundary(turn_id, item_id))?;
    let turn_id = turn_id.to_string();
    let item_id = item_id.to_string();
    let (cancelled, lifetime) = tokio::sync::oneshot::channel::<()>();
    let position = tokio::task::spawn_blocking(move || {
        let file = codex_rollout::open_rollout_seekable_reader(&path, || cancelled.is_closed())
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to open prompt boundary source: {error}"),
            })?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(start_offset))
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to seek prompt boundary source: {error}"),
            })?;
        let mut offset = start_offset;
        let mut line = String::new();
        let mut source = UserPromptSource::default();
        let mut presentations = Vec::new();
        let mut selected = None;
        while offset < end_offset {
            if cancelled.is_closed() {
                return Err(invalid_boundary(&turn_id, &item_id));
            }
            line.clear();
            let bytes = reader.read_line(&mut line)
                .map_err(|error| ThreadStoreError::Internal {
                    message: format!("failed to read prompt boundary source: {error}"),
                })?;
            if bytes == 0 {
                return Err(invalid_boundary(&turn_id, &item_id));
            }
            let next_offset = offset
                .checked_add(bytes as u64)
                .ok_or_else(|| invalid_boundary(&turn_id, &item_id))?;
            let record = codex_rollout::parse_rollout_line(&line)
                .map_err(|_| invalid_boundary(&turn_id, &item_id))?;
            let record_ordinal = record.ordinal
                .ok_or_else(|| invalid_boundary(&turn_id, &item_id))?;
            if next_offset > end_offset
                || (offset == start_offset && !matches!(
                    &record.item,
                    RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == turn_id
                ))
            {
                return Err(invalid_boundary(&turn_id, &item_id));
            }
            let position = (record_ordinal, offset);
            source.observe(&record.item, position);
            if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = record.item
                && event.turn_id == turn_id
                && let TurnItem::UserMessage(user) = event.item
            {
                if user.id == item_id {
                    if selected.is_some() || record_ordinal != ordinal {
                        return Err(invalid_boundary(&turn_id, &item_id));
                    }
                    selected = Some(presentations.len());
                }
                presentations.push(position);
            }
            offset = next_offset;
        }
        let (ordinal, offset) = source
            .boundary(
                &presentations,
                selected.ok_or_else(|| invalid_boundary(&turn_id, &item_id))?,
            )
            .ok_or_else(|| invalid_boundary(&turn_id, &item_id))?;
        Ok(HistoryPosition {
            thread_id: rollout_id,
            end_ordinal_exclusive: ordinal,
            end_byte_offset: offset,
        })
    })
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("prompt boundary reader failed: {error}"),
    })?;
    drop(lifetime);
    position
}

fn invalid_boundary(turn_id: &str, item_id: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!(
            "user-message item {item_id} has no visible source boundary in turn {turn_id}"
        ),
    }
}
