//! Bounded layout caches for settled exec cells. Live cells never populate these entries.
//!
//! Keep one width per presentation mode, not one entry per resize. The owner replaces the cache
//! on every content/status mutation; hits therefore need not walk calls to check finalization.

use std::sync::Mutex;

use ratatui::text::Line;

#[derive(Debug, Default)]
pub(super) struct RenderCache {
    review: Mutex<Option<RenderedLines>>,
    full: Mutex<Option<RenderedLines>>,
}

#[derive(Clone, Copy)]
pub(super) enum RenderMode {
    Review,
    Full,
}

#[derive(Debug)]
struct RenderedLines {
    width: u16,
    theme_revision: u64,
    lines: Vec<Line<'static>>,
}

impl RenderCache {
    pub(super) fn render(
        &self,
        mode: RenderMode,
        width: u16,
        theme_revision: u64,
        is_active: impl FnOnce() -> bool,
        render: impl FnOnce() -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let slot = match mode {
            RenderMode::Review => &self.review,
            RenderMode::Full => &self.full,
        };
        let mut entry = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = entry.as_ref()
            && cached.width == width
            && cached.theme_revision == theme_revision
        {
            return cached.lines.clone();
        }
        let cacheable = !is_active();
        let lines = render();
        if cacheable {
            *entry = Some(RenderedLines {
                width,
                theme_revision,
                lines: lines.clone(),
            });
        }
        lines
    }
}

#[cfg(test)]
#[path = "render_cache_tests.rs"]
mod tests;
