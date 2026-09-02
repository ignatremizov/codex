//! Bounded committed-cell windows for transcript overlays.
//!
//! The overlay retains every loaded cell for exact navigation and backtrack identity, but only
//! materializes one overlapping window at a time. This keeps wrapping and dynamic layout polling
//! independent of the total amount of history loaded into the thread.

use super::TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT;
use super::TRANSCRIPT_RENDER_WINDOW_OVERLAP;
use super::TranscriptOverlay;
use super::ViewportAnchor;

impl TranscriptOverlay {
    pub(super) fn rendered_cell_count(&self) -> usize {
        self.render_end.saturating_sub(self.render_start)
    }

    pub(super) fn renders_loaded_start(&self) -> bool {
        self.render_start == 0
    }

    pub(super) fn renders_loaded_end(&self) -> bool {
        self.render_end == self.cells.len()
    }

    pub(super) fn global_viewport_anchor(&self) -> Option<ViewportAnchor> {
        let anchor = self.view.viewport_anchor()?;
        if anchor.chunk_index < self.rendered_cell_count() {
            return Some(ViewportAnchor {
                chunk_index: self.render_start.saturating_add(anchor.chunk_index),
                row_offset: anchor.row_offset,
            });
        }
        (self.renders_loaded_end() && anchor.chunk_index == self.rendered_cell_count()).then_some(
            ViewportAnchor {
                chunk_index: self.cells.len(),
                row_offset: anchor.row_offset,
            },
        )
    }

    pub(super) fn local_viewport_anchor(&self, anchor: ViewportAnchor) -> Option<ViewportAnchor> {
        if anchor.chunk_index == self.cells.len() && self.renders_loaded_end() {
            return Some(ViewportAnchor {
                chunk_index: self.rendered_cell_count(),
                row_offset: anchor.row_offset,
            });
        }
        (self.render_start..self.render_end)
            .contains(&anchor.chunk_index)
            .then_some(ViewportAnchor {
                chunk_index: anchor.chunk_index.saturating_sub(self.render_start),
                row_offset: anchor.row_offset,
            })
    }

    pub(super) fn update_scroll_percentage_visibility(&mut self) {
        self.view.scroll_percentage_visible = !self.history_state.has_unloaded_history()
            && self.renders_loaded_start()
            && self.renders_loaded_end();
    }

    pub(super) fn clear_pending_view_navigation(&mut self) {
        self.view.pending_scroll_chunk = None;
        self.view.pending_align_chunk_top = None;
        self.view.pending_viewport_anchor = None;
    }

    pub(super) fn show_loaded_start(&mut self) {
        let tail_renderable = self.take_live_tail_renderable();
        self.clear_pending_view_navigation();
        self.render_start = 0;
        self.render_end = self.cells.len().min(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT);
        self.rebuild_renderables_from_global_anchor(/*viewport_anchor*/ None, tail_renderable);
        self.view.scroll_offset = 0;
    }

    pub(super) fn show_loaded_end(&mut self) {
        let tail_renderable = self.take_live_tail_renderable();
        self.clear_pending_view_navigation();
        self.render_end = self.cells.len();
        self.render_start = self
            .render_end
            .saturating_sub(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT);
        self.rebuild_renderables_from_global_anchor(/*viewport_anchor*/ None, tail_renderable);
        self.view.scroll_offset = usize::MAX;
    }

    pub(super) fn show_window_containing(&mut self, global_index: usize) {
        if (self.render_start..self.render_end).contains(&global_index) {
            return;
        }
        let tail_renderable = self.take_live_tail_renderable();
        self.clear_pending_view_navigation();
        self.render_start = global_index.saturating_sub(TRANSCRIPT_RENDER_WINDOW_OVERLAP);
        self.render_end = self
            .render_start
            .saturating_add(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT)
            .min(self.cells.len());
        if self.render_end == self.cells.len() {
            self.render_start = self
                .render_end
                .saturating_sub(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT);
        }
        self.rebuild_renderables_from_global_anchor(/*viewport_anchor*/ None, tail_renderable);
    }

    pub(super) fn show_previous_window(&mut self) -> bool {
        if self.render_start == 0 {
            return false;
        }
        let tail_renderable = self.take_live_tail_renderable();
        self.clear_pending_view_navigation();
        self.render_end = self
            .render_start
            .saturating_add(TRANSCRIPT_RENDER_WINDOW_OVERLAP)
            .min(self.cells.len());
        self.render_start = self
            .render_end
            .saturating_sub(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT);
        self.rebuild_renderables_from_global_anchor(/*viewport_anchor*/ None, tail_renderable);
        self.view.scroll_offset = usize::MAX;
        true
    }

    pub(super) fn show_next_window(&mut self) -> bool {
        if self.render_end == self.cells.len() {
            return false;
        }
        let tail_renderable = self.take_live_tail_renderable();
        self.clear_pending_view_navigation();
        self.render_start = self
            .render_end
            .saturating_sub(TRANSCRIPT_RENDER_WINDOW_OVERLAP);
        self.render_end = self
            .render_start
            .saturating_add(TRANSCRIPT_RENDER_WINDOW_CELL_LIMIT)
            .min(self.cells.len());
        self.rebuild_renderables_from_global_anchor(/*viewport_anchor*/ None, tail_renderable);
        self.view.scroll_offset = 0;
        true
    }
}
