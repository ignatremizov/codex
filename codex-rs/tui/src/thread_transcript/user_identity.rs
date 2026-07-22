//! Carry canonical prompt identity through pure transcript projection.

use super::*;
use crate::history_cell::UserMessageIdentity;

pub(crate) fn attach_projected_user_identities<'a>(
    cells: &[Arc<dyn HistoryCell>],
    items: impl IntoIterator<Item = (Option<&'a str>, &'a ThreadItem)>,
) {
    let users = items.into_iter().filter_map(|(turn_id, item)| {
        let ThreadItem::UserMessage { id, .. } = item else {
            return None;
        };
        Some(turn_id.map(|turn_id| UserMessageIdentity {
            turn_id: turn_id.to_string(),
            item_id: id.clone(),
        }))
    });
    for (cell, identity) in cells
        .iter()
        .filter_map(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
        .zip(users)
    {
        if let Some(identity) = identity {
            let _ = cell.identity.set(identity);
        }
    }
}

#[cfg(test)]
#[path = "user_identity_tests.rs"]
mod tests;
