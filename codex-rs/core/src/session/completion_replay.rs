//! Idempotent model-context replay across completion delivery and compaction checkpoints.

use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use std::collections::HashMap;
use std::collections::HashSet;

pub(super) fn trusted_contexts(
    items: &[RolloutItem],
    removed: &[bool],
) -> HashMap<ResponseItemId, ResponseItemEnvelope> {
    let mut trusted = HashMap::new();
    let mut conflicting = HashSet::new();
    for (index, pair) in items.windows(2).enumerate() {
        if removed[index] || removed[index + 1] {
            continue;
        }
        let [
            RolloutItem::InterAgentCommunicationMetadata { .. },
            RolloutItem::ResponseItem(item),
        ] = pair
        else {
            continue;
        };
        let Some(id) = item.id().filter(|id| {
            is_sub_agent_completion_context_response_item_id(id.as_str())
                || codex_history::is_committed_observed_response(items, index + 1)
        }) else {
            continue;
        };
        if trusted.get(id).is_some_and(|previous| previous != item) {
            conflicting.insert(id.clone());
        } else {
            trusted.insert(id.clone(), item.clone());
        }
    }
    for item in items {
        if let RolloutItem::ResponseItem(item) = item
            && let Some(id) = item.id()
            && trusted.get(id).is_some_and(|previous| previous != item)
        {
            conflicting.insert(id.clone());
        }
    }
    trusted.retain(|id, _| !conflicting.contains(id));
    trusted
}

pub(super) fn context_ids(
    items: &[ResponseItemEnvelope],
    trusted: &HashMap<ResponseItemId, ResponseItemEnvelope>,
) -> HashSet<ResponseItemId> {
    items
        .iter()
        .filter(|item| item.id().and_then(|id| trusted.get(id)) == Some(*item))
        .filter_map(|item| item.id())
        .cloned()
        .collect()
}

pub(super) fn deduplicate(
    items: &mut Vec<ResponseItemEnvelope>,
    prefix_len: usize,
    trusted: &HashMap<ResponseItemId, ResponseItemEnvelope>,
) -> (usize, bool) {
    let mut seen = HashSet::new();
    let mut index = 0;
    let mut retained_prefix_len = 0;
    items.retain(|item| {
        let retain = item
            .id()
            .filter(|id| trusted.get(*id) == Some(item))
            .is_none_or(|id| seen.insert(id.clone()));
        if retain && index < prefix_len {
            retained_prefix_len += 1;
        }
        index += 1;
        retain
    });
    (retained_prefix_len, index != items.len())
}
