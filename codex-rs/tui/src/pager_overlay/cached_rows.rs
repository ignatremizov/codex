//! Width-keyed immutable text rows; scrolling clones and paints only the visible slice.

use std::cell::RefCell;

use crate::render::renderable::Renderable;
use crate::terminal_hyperlinks::wrap_line_rows;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

pub(super) struct CachedRows {
    lines: Vec<Line<'static>>,
    cache: RefCell<Option<(u16, Vec<Line<'static>>)>>,
}

impl CachedRows {
    pub(super) fn new(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            cache: RefCell::new(None),
        }
    }

    fn with_rows<T>(&self, width: u16, f: impl FnOnce(&[Line<'static>]) -> T) -> T {
        let mut cache = self.cache.borrow_mut();
        if cache
            .as_ref()
            .is_some_and(|(cached_width, _)| *cached_width != width)
        {
            cache.take();
        }
        let (_, rows) = cache.get_or_insert_with(|| {
            (
                width,
                self.lines
                    .iter()
                    .flat_map(|line| wrap_line_rows(line, width, Style::default()))
                    .collect(),
            )
        });
        f(rows)
    }
}

impl Renderable for CachedRows {
    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.desired_height_usize(width)).unwrap_or(u16::MAX)
    }

    fn desired_height_usize(&self, width: u16) -> usize {
        self.with_rows(width, <[Line<'static>]>::len)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_with_offset(area, buf, /*scroll_offset*/ 0);
    }

    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        self.with_rows(area.width, |rows| {
            let start = scroll_offset.min(rows.len());
            let end = start
                .saturating_add(usize::from(area.height))
                .min(rows.len());
            // Rows already contain alignment padding. Do not wrap or align them again.
            Widget::render(Paragraph::new(rows[start..end].to_vec()), area, buf);
        });
    }
}
