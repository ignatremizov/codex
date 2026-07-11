//! Viewport fallback for generic static pager content.

use crate::render::renderable::Renderable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Render visible rows directly when supported, preserving the legacy scratch-buffer fallback.
pub(super) fn render_offset_content(
    area: Rect,
    buf: &mut Buffer,
    renderable: &dyn Renderable,
    scroll_offset: usize,
) -> u16 {
    let height = renderable.desired_height_usize(area.width);
    let copy_height = height
        .saturating_sub(scroll_offset)
        .min(usize::from(area.height)) as u16;
    if copy_height == 0 {
        return 0;
    }

    let visible_area = Rect::new(area.x, area.y, area.width, copy_height);
    renderable.render_with_offset(visible_area, buf, scroll_offset);

    copy_height
}

#[cfg(test)]
#[path = "scrolling_tests.rs"]
mod tests;
