//! Refresh list data without resetting a user's filter, selection, or viewport.

use super::ListSelectionView;
use super::SelectionViewParams;
use crate::keymap::ListKeymap;

impl ListSelectionView {
    pub(super) fn refresh(&mut self, mut params: SelectionViewParams, keymap: ListKeymap) {
        self.sync_last_rendered_scroll_top();
        let selected = params.initial_selected_idx.or(self.selected_actual_idx());
        params.initial_selected_idx = selected;
        if params.initial_tab_id.is_none() {
            params.initial_tab_id = self.active_tab_id().map(str::to_owned);
        }

        // Construction must not trigger an intermediate unfiltered selection.
        let on_selection_changed = params.on_selection_changed.take();
        let mut refreshed = Self::new(params, self.app_event_tx.clone(), keymap);
        if refreshed.is_searchable {
            refreshed.search_query = std::mem::take(&mut self.search_query);
        }
        refreshed.state.selected_idx = None;
        refreshed.state.scroll_top = self.state.scroll_top;
        refreshed.filtered_indices.clear();
        refreshed.initial_selected_idx = selected;
        refreshed
            .last_rendered_visible_rows
            .set(self.last_rendered_visible_rows.get());
        refreshed
            .last_rendered_scroll_top
            .set(self.state.scroll_top);
        refreshed.apply_filter();
        refreshed.dismiss_after_child_accept = self.dismiss_after_child_accept;
        refreshed.on_selection_changed = on_selection_changed;
        if refreshed.selected_actual_idx() != selected {
            refreshed.fire_selection_changed();
        }
        *self = refreshed;
    }
}

#[cfg(test)]
#[path = "list_selection_view_refresh_tests.rs"]
mod tests;
