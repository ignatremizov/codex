use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::rollout::exact_rollback_removed_items;

use crate::StoredSubAgentCompletionPresentation;

pub(crate) fn context_item(
    items: &[RolloutItem],
    response_item_id: &ResponseItemId,
) -> Option<ResponseItem> {
    if !is_sub_agent_completion_context_response_item_id(response_item_id.as_str()) {
        return None;
    }
    let removed = exact_rollback_removed_items(items);
    items
        .windows(2)
        .enumerate()
        .rev()
        .find_map(|(index, pair)| {
            if removed[index] || removed[index.saturating_add(1)] {
                return None;
            }
            let [
                RolloutItem::InterAgentCommunicationMetadata { .. },
                RolloutItem::ResponseItem(item),
            ] = pair
            else {
                return None;
            };
            (item.id() == Some(response_item_id)).then(|| item.clone())
        })
}

pub(crate) fn presentation(
    items: &[RolloutItem],
    item_id: &str,
    turn_id: &str,
) -> StoredSubAgentCompletionPresentation {
    let removed = exact_rollback_removed_items(items);
    let mut presentation = StoredSubAgentCompletionPresentation::default();
    let mut completion_in_queried_turn = None;
    let mut completion_in_other_turn = None;
    for (index, item) in items.iter().enumerate() {
        if removed[index] {
            continue;
        }
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == turn_id => {
                presentation.turn_started = true;
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.item.id() == item_id
                    && event.item.is_sub_agent_completion_presentation() =>
            {
                if event.turn_id == turn_id {
                    completion_in_queried_turn = Some(event.clone());
                } else {
                    completion_in_other_turn = Some(event.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == turn_id => {
                presentation.turn_completed = true;
            }
            _ => {}
        }
    }
    presentation.item_completed = completion_in_queried_turn.or(completion_in_other_turn);
    presentation
}
