//! Prefix-indexed static pager; only visible chunks paint each frame.

use super::scrolling::render_offset_content;
use super::*;
use ratatui::buffer::Cell;

/// Generic widget for rendering a pager view.
pub(super) struct PagerView {
    renderables: Vec<Box<dyn Renderable>>,
    layout_width: Option<u16>,
    theme_revision: u64,
    chunk_bottoms: Vec<usize>,
    dynamic_revisions: Vec<(usize, u64)>,
    scroll_offset: usize,
    title: String,
    pub(super) keymap: PagerKeymap,
    last_content_height: Option<usize>,
}

impl PagerView {
    pub(super) fn new(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        scroll_offset: usize,
        keymap: PagerKeymap,
    ) -> Self {
        let dynamic_revisions = renderables
            .iter()
            .enumerate()
            .filter_map(|(index, child)| child.layout_revision().map(|revision| (index, revision)))
            .collect();
        Self {
            renderables,
            layout_width: None,
            theme_revision: crate::render::highlight::syntax_theme_revision(),
            chunk_bottoms: Vec::new(),
            dynamic_revisions,
            scroll_offset,
            title,
            keymap,
            last_content_height: None,
        }
    }

    fn content_height(&mut self, width: u16) -> usize {
        let theme = crate::render::highlight::syntax_theme_revision();
        let mut changed = None;
        for (index, previous) in &mut self.dynamic_revisions {
            if let Some(revision) = self.renderables[*index].layout_revision()
                && revision != *previous
            {
                *previous = revision;
                changed = Some(changed.map_or(*index, |first: usize| first.min(*index)));
            }
        }
        if self.layout_width != Some(width) || self.theme_revision != theme {
            changed = Some(0);
        }
        if let Some(first) = changed {
            self.chunk_bottoms.truncate(first);
            let mut bottom = self.chunk_bottoms.last().copied().unwrap_or(0);
            for child in self.renderables.iter().skip(first) {
                bottom = bottom.saturating_add(child.desired_height_usize(width));
                self.chunk_bottoms.push(bottom);
            }
            self.layout_width = Some(width);
            self.theme_revision = theme;
        }
        self.chunk_bottoms.last().copied().unwrap_or(0)
    }

    pub(super) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        Clear.render(area, buf);
        self.render_header(area, buf);
        let content_area = self.content_area(area);
        self.update_last_content_height(content_area.height);
        let content_height = self.content_height(content_area.width);
        self.scroll_offset = self
            .scroll_offset
            .min(content_height.saturating_sub(content_area.height as usize));

        self.render_content(content_area, buf);

        self.render_bottom_bar(area, content_area, buf, content_height);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        Span::from("/ ".repeat(area.width as usize / 2))
            .dim()
            .render(area, buf);
        let header = format!("/ {}", self.title);
        header.dim().render(area, buf);
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        let first = self
            .chunk_bottoms
            .partition_point(|bottom| *bottom <= self.scroll_offset);
        let mut top = first
            .checked_sub(1)
            .map_or(0, |index| self.chunk_bottoms[index]);
        let mut drawn_bottom = area.y;
        for (index, renderable) in self.renderables.iter().enumerate().skip(first) {
            let bottom = self.chunk_bottoms[index];
            let visible_top = top.saturating_sub(self.scroll_offset);
            if visible_top >= usize::from(area.height) {
                break;
            }
            let offset = self.scroll_offset.saturating_sub(top);
            let y = area.y.saturating_add(visible_top as u16);
            let height = bottom
                .saturating_sub(top)
                .saturating_sub(offset)
                .min(usize::from(area.bottom().saturating_sub(y))) as u16;
            let draw_area = Rect::new(area.x, y, area.width, height);
            render_offset_content(draw_area, buf, &**renderable, offset);
            drawn_bottom = drawn_bottom.max(y.saturating_add(height));
            top = bottom;
        }

        for y in drawn_bottom..area.bottom() {
            if area.width == 0 {
                break;
            }
            buf[(area.x, y)] = Cell::from('~');
            for x in area.x + 1..area.right() {
                buf[(x, y)] = Cell::from(' ');
            }
        }
    }

    fn render_bottom_bar(
        &self,
        full_area: Rect,
        content_area: Rect,
        buf: &mut Buffer,
        total_len: usize,
    ) {
        if full_area.height < 2 {
            return;
        }
        let sep_y = content_area.bottom();
        let sep_rect = Rect::new(full_area.x, sep_y, full_area.width, /*height*/ 1);

        Span::from("─".repeat(sep_rect.width as usize))
            .dim()
            .render(sep_rect, buf);
        let percent = if total_len == 0 {
            100
        } else {
            let max_scroll = total_len.saturating_sub(content_area.height as usize);
            if max_scroll == 0 {
                100
            } else {
                (((self.scroll_offset.min(max_scroll)) as f32 / max_scroll as f32) * 100.0).round()
                    as u8
            }
        };
        let pct_text = format!(" {percent}% ");
        let pct_w = pct_text.chars().count() as u16;
        let pct_x = sep_rect
            .x
            .saturating_add(sep_rect.width.saturating_sub(pct_w.saturating_add(1)));
        Span::from(pct_text).dim().render(
            Rect::new(
                pct_x,
                sep_rect.y,
                pct_w.min(sep_rect.width),
                /*height*/ 1,
            ),
            buf,
        );
    }

    pub(super) fn handle_key_event(
        &mut self,
        tui: &mut tui::Tui,
        key_event: KeyEvent,
    ) -> Result<()> {
        match key_event {
            e if self.keymap.scroll_up.is_pressed(e) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            e if self.keymap.scroll_down.is_pressed(e) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            e if self.keymap.page_up.is_pressed(e) => {
                let page_height = self.page_height(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_sub(page_height);
            }
            e if self.keymap.page_down.is_pressed(e) => {
                let page_height = self.page_height(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_add(page_height);
            }
            e if self.keymap.half_page_down.is_pressed(e) => {
                let half_page = self
                    .page_height(tui.terminal.viewport_area)
                    .saturating_add(1)
                    / 2;
                self.scroll_offset = self.scroll_offset.saturating_add(half_page);
            }
            e if self.keymap.half_page_up.is_pressed(e) => {
                let half_page = self
                    .page_height(tui.terminal.viewport_area)
                    .saturating_add(1)
                    / 2;
                self.scroll_offset = self.scroll_offset.saturating_sub(half_page);
            }
            e if self.keymap.jump_top.is_pressed(e) => {
                self.scroll_offset = 0;
            }
            e if self.keymap.jump_bottom.is_pressed(e) => {
                self.scroll_offset = usize::MAX;
            }
            _ => {
                return Ok(());
            }
        }
        tui.frame_requester()
            .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
        Ok(())
    }

    /// Returns the height of one page in content rows.
    ///
    /// Prefers the last rendered content height (excluding header/footer chrome);
    /// if no render has occurred yet, falls back to the content area height
    /// computed from the given viewport.
    fn page_height(&self, viewport_area: Rect) -> usize {
        self.last_content_height
            .unwrap_or_else(|| self.content_area(viewport_area).height as usize)
    }

    fn update_last_content_height(&mut self, height: u16) {
        self.last_content_height = Some(height as usize);
    }

    fn content_area(&self, area: Rect) -> Rect {
        let mut area = area;
        area.y = area.y.saturating_add(1);
        area.height = area.height.saturating_sub(2);
        area
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
