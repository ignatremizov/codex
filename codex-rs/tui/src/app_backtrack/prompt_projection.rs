//! Editable prompts across fork headers, keyed by receipt identity rather than visible text.

use crate::history_cell::HistoryCell;
use crate::history_cell::SessionInfoCell;
use crate::history_cell::UserHistoryCell;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn user_count(cells: &[Arc<dyn HistoryCell>]) -> usize {
    user_positions_iter(cells).count()
}

pub(crate) fn nth_user_position(cells: &[Arc<dyn HistoryCell>], nth: usize) -> Option<usize> {
    user_positions_iter(cells).nth(nth)
}

/// Trim a confirmed visible suffix without reviving older copies of removed canonical prompts.
pub(crate) fn truncate_before_prompt(cells: &mut Vec<Arc<dyn HistoryCell>>, nth: usize) -> bool {
    let positions = user_positions_iter(cells).collect::<Vec<_>>();
    let Some(&cut) = positions.get(nth) else {
        return false;
    };
    let visible: HashSet<_> = positions.into_iter().collect();
    let header = cells[cut..]
        .iter()
        .rev()
        .find(|cell| cell.as_any().is::<SessionInfoCell>())
        .cloned();
    *cells = cells[..cut]
        .iter()
        .enumerate()
        .filter(|(index, cell)| {
            cell.as_any()
                .downcast_ref::<UserHistoryCell>()
                .is_none_or(|user| {
                    user.identity.get().is_none()
                        || !user.has_visible_content()
                        || visible.contains(index)
                })
        })
        .map(|(_, cell)| Arc::clone(cell))
        .chain(header)
        .collect();
    true
}

pub(crate) fn user_positions_iter(
    cells: &[Arc<dyn HistoryCell>],
) -> impl Iterator<Item = usize> + '_ {
    let current_session_start = cells
        .iter()
        .rposition(|cell| cell.as_any().is::<SessionInfoCell>())
        .map_or(0, |idx| idx + 1);
    let mut seen_identities = HashSet::new();
    let mut positions = Vec::new();
    for (idx, cell) in cells.iter().enumerate().rev() {
        let Some(user) = cell.as_any().downcast_ref::<UserHistoryCell>() else {
            continue;
        };
        if !user.has_visible_content() {
            continue;
        }
        let included = match user.identity.get() {
            // Inherited canonical prompts remain editable before a fork's session header.
            // Replay can render the same persisted item again; prefer its newest visible copy.
            Some(identity) => seen_identities.insert(identity),
            // An optimistic prompt has no durable identity yet. Keep its session boundary so
            // stale source-less copies cannot shift the ordinal fallback.
            None => idx >= current_session_start,
        };
        if included {
            positions.push(idx);
        }
    }
    positions.reverse();
    positions.into_iter()
}
