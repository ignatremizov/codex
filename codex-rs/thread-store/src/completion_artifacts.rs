use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_rollout::RolloutItem;
use codex_rollout::exact_rollback_removed_items;

use crate::StoredSubAgentCompletionPresentation;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Keep raw source segments separate: masks and metadata adjacency use original coordinates.
pub(crate) fn context_item(
    segments: &[Vec<RolloutItem>],
    response_item_id: &ResponseItemId,
) -> ThreadStoreResult<Option<ResponseItem>> {
    if !is_sub_agent_completion_context_response_item_id(response_item_id.as_str()) {
        return Err(ThreadStoreError::InvalidRequest {
            message: "invalid reserved completion context identity".to_string(),
        });
    }
    let mut found = None;
    for items in segments {
        let removed = exact_rollback_removed_items(items);
        for (index, item) in items.iter().enumerate() {
            if removed[index] {
                continue;
            }
            let RolloutItem::ResponseItem(item) = item else {
                continue;
            };
            if item.id() != Some(response_item_id) {
                continue;
            }
            if !matches!(
                index
                    .checked_sub(1)
                    .filter(|previous| !removed[*previous])
                    .and_then(|previous| items.get(previous)),
                Some(RolloutItem::InterAgentCommunicationMetadata { .. })
            ) || found
                .as_ref()
                .is_some_and(|previous| previous != &item.item)
            {
                return Err(ThreadStoreError::Conflict {
                    message: format!(
                        "completion context {response_item_id} has conflicting provenance or payload"
                    ),
                });
            }
            found = Some(item.item.clone());
        }
    }
    Ok(found)
}

pub(crate) fn presentation(
    segments: &[Vec<RolloutItem>],
    item_id: &str,
    turn_id: &str,
) -> ThreadStoreResult<StoredSubAgentCompletionPresentation> {
    let mut presentation = StoredSubAgentCompletionPresentation::default();
    for items in segments {
        let removed = exact_rollback_removed_items(items);
        for (item, removed) in items.iter().zip(removed) {
            if removed {
                continue;
            }
            match item {
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == turn_id => {
                    presentation.turn_started = true;
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.item.id() == item_id =>
                {
                    if event.turn_id != turn_id
                        || !event.item.is_sub_agent_completion_presentation()
                    {
                        return Err(ThreadStoreError::Conflict {
                            message: format!(
                                "completion {item_id} has conflicting turn or provenance"
                            ),
                        });
                    }
                    if let Some(previous) = presentation.item_completed.as_ref() {
                        let serialize = |event: &codex_protocol::protocol::ItemCompletedEvent| {
                            serde_json::to_value(event).map_err(|err| ThreadStoreError::Internal {
                                message: format!("failed to compare completion payload: {err}"),
                            })
                        };
                        if serialize(previous)? != serialize(event)? {
                            return Err(ThreadStoreError::Conflict {
                                message: format!("completion {item_id} has conflicting payloads"),
                            });
                        }
                    }
                    presentation.item_completed = Some(event.clone());
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == turn_id =>
                {
                    presentation.turn_completed = true;
                }
                _ => {}
            }
        }
    }
    Ok(presentation)
}

#[cfg(test)]
#[path = "completion_artifacts_tests.rs"]
mod tests;
