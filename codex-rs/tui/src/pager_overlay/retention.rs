//! Reuse is limited to the current committed window, never an off-window history cache.
//!
//! Cell identity, presentation mode, and style determine wrapped rows. Window-edge spacing is
//! owned by the outer wrapper so moving a cell to or from the first slot retains those rows.

use std::rc::Rc;
use std::sync::Arc;

use super::CachedRenderable;
use super::CellRenderable;
use super::TranscriptOverlay;
use crate::history_cell::SessionInfoCell;
use crate::history_cell::UserHistoryCell;
use crate::render::Insets;
use crate::render::renderable::InsetRenderable;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;

pub(super) struct RetainedCell {
    inner: Rc<CellRenderable>,
    placeholder: Option<&'static str>,
}

impl RetainedCell {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner.cell, &other.inner.cell)
            && self.inner.style == other.inner.style
            && self.inner.detail_mode == other.inner.detail_mode
            && self.placeholder == other.placeholder
    }

    pub(super) fn renderable(&self, local_index: usize) -> Box<dyn Renderable> {
        if let Some(placeholder) = self.placeholder {
            return Box::new(Line::from(placeholder).dim());
        }
        let shared = SharedCell(Rc::clone(&self.inner));
        let renderable: Box<dyn Renderable> = if self.inner.has_stable_layout {
            Box::new(CachedRenderable::new(shared))
        } else {
            Box::new(shared)
        };
        if local_index > 0 && !self.inner.cell.is_stream_continuation() {
            Box::new(InsetRenderable::new(
                renderable,
                Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                ),
            ))
        } else {
            renderable
        }
    }
}

struct SharedCell(Rc<CellRenderable>);

impl Renderable for SharedCell {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.0.render(area, buf);
    }

    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        self.0.render_with_offset(area, buf, scroll_offset);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }

    fn desired_height_usize(&self, width: u16) -> usize {
        self.0.desired_height_usize(width)
    }

    fn layout_revision(&self) -> Option<u64> {
        self.0.layout_revision()
    }
}

impl TranscriptOverlay {
    pub(super) fn retained_cell(&self, global_index: usize) -> RetainedCell {
        let cell = &self.cells[global_index];
        let style = if cell.as_any().is::<UserHistoryCell>() {
            if self.highlight_cell == Some(global_index) {
                user_message_style().reversed()
            } else {
                user_message_style()
            }
        } else {
            Style::default()
        };
        RetainedCell {
            inner: Rc::new(CellRenderable::new(
                Arc::clone(cell),
                style,
                self.browser.detail_mode(),
            )),
            placeholder: cell
                .as_any()
                .is::<SessionInfoCell>()
                .then_some(self.history_state.session_header_placeholder())
                .flatten(),
        }
    }

    pub(super) fn reconcile_renderables(&mut self) {
        let next = (self.render_start..self.render_end)
            .map(|index| self.retained_cell(index))
            .collect::<Vec<_>>();
        if next.len() == self.retained_cells.len()
            && next
                .iter()
                .zip(&self.retained_cells)
                .all(|(next, previous)| next.matches(previous))
        {
            return;
        }

        let mut previous = std::mem::take(&mut self.retained_cells)
            .into_iter()
            .zip(std::mem::take(&mut self.view.renderables))
            .enumerate()
            .map(Some)
            .collect::<Vec<_>>();
        let mut renderables = Vec::with_capacity(next.len());
        for (local_index, next) in next.into_iter().enumerate() {
            let reusable = previous.iter().position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|(_, (retained, _))| retained.matches(&next))
            });
            let (retained, renderable) = if let Some((old_index, (retained, renderable))) =
                reusable.and_then(|index| previous[index].take())
            {
                let renderable = if (old_index == 0) == (local_index == 0) {
                    renderable
                } else {
                    retained.renderable(local_index)
                };
                (retained, renderable)
            } else {
                let renderable = next.renderable(local_index);
                (next, renderable)
            };
            self.retained_cells.push(retained);
            renderables.push(renderable);
        }
        self.view.replace_renderables(renderables);
    }

    pub(super) fn live_tail_is_visible(&self, width: u16) -> bool {
        if self.view.is_scrolled_to_bottom() {
            return true;
        }
        // A resize invalidates old row coordinates. Refresh conservatively rather than using
        // those coordinates to suppress a tail that may now be visible.
        if self
            .view
            .layout_width
            .is_some_and(|previous| previous != width)
        {
            return true;
        }
        let Some(height) = self.view.last_content_height else {
            return false;
        };
        let committed = self.rendered_cell_count();
        let pending_chunk = self
            .view
            .pending_align_chunk_top
            .or(self.view.pending_scroll_chunk)
            .or(self
                .view
                .pending_viewport_anchor
                .map(|anchor| anchor.chunk_index));
        let Some(tail_top) = committed
            .checked_sub(1)
            .and_then(|index| self.view.chunk_bottoms.get(index))
            .copied()
        else {
            return committed == 0 || pending_chunk.is_some_and(|index| index >= committed);
        };
        let offset = if let Some(index) = pending_chunk {
            let top = index
                .checked_sub(1)
                .and_then(|index| self.view.chunk_bottoms.get(index))
                .copied()
                .unwrap_or(0);
            top.saturating_add(
                self.view
                    .pending_viewport_anchor
                    .map_or(0, |anchor| anchor.row_offset),
            )
        } else {
            self.view.scroll_offset
        };
        offset.saturating_add(height) > tail_top
    }
}
