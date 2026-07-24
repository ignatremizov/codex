//! Overlay UIs rendered in an alternate screen.
//!
//! This module implements the pager-style overlays used by the TUI, including the transcript
//! overlay (`Ctrl+T`) that renders a full history view separate from the main viewport.
//!
//! The transcript overlay renders committed transcript cells plus an optional render-only live tail
//! derived from the current in-flight active cell. Because rebuilding wrapped `Line`s on every draw
//! can be expensive, that live tail is cached and only recomputed when its cache key changes, which
//! is derived from the terminal width (wrapping), an active-cell revision (in-place mutations), the
//! stream-continuation flag (spacing), and an animation tick (time-based spinner/shimmer output).
//!
//! The transcript overlay live tail is kept in sync by `App` during draws: `App` supplies an
//! `ActiveCellTranscriptKey` and a function to compute the active cell transcript lines, and
//! `TranscriptOverlay::sync_live_tail` uses the key to decide when the cached tail must be
//! recomputed. `ChatWidget` is responsible for producing a key that changes when the active cell
//! mutates in place or when its transcript output is time-dependent.

mod transcript;

use std::io::Result;
use std::sync::Arc;

use self::transcript::TranscriptBrowserState;
use self::transcript::TranscriptDetailMode;
use self::transcript::TranscriptFlavor;
use self::transcript::transcript_title;
use crate::chatwidget::ActiveCellTranscriptKey;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::key_hint::KeyBinding;
use crate::key_hint::KeyBindingListExt;
use crate::keymap::PagerKeymap;
use crate::render::Insets;
use crate::render::renderable::InsetRenderable;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::mark_buffer_hyperlinks_in_rows;
use crate::terminal_hyperlinks::visible_lines;
use crate::terminal_hyperlinks::wrap_hyperlink_lines;
use crate::tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;

pub(crate) enum Overlay {
    Transcript(TranscriptOverlay),
    Static(StaticOverlay),
}

impl Overlay {
    pub(crate) fn new_transcript(cells: Vec<Arc<dyn HistoryCell>>, keymap: PagerKeymap) -> Self {
        Self::Transcript(TranscriptOverlay::new(
            cells,
            keymap,
            TranscriptFlavor::HistoricalFullPreview,
        ))
    }

    pub(crate) fn new_review_transcript(
        cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Transcript(TranscriptOverlay::new(
            cells,
            keymap,
            TranscriptFlavor::LiveReviewBrowser,
        ))
    }

    pub(crate) fn new_static_with_lines(
        lines: Vec<Line<'static>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Static(StaticOverlay::with_title(lines, title, keymap))
    }

    pub(crate) fn new_static_with_renderables(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Static(StaticOverlay::with_renderables(renderables, title, keymap))
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match self {
            Overlay::Transcript(o) => o.handle_event(tui, event),
            Overlay::Static(o) => o.handle_event(tui, event),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_done(),
            Overlay::Static(o) => o.is_done(),
        }
    }

    pub(crate) fn is_transcript_close_key(&self, key_event: KeyEvent) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_close_key(key_event),
            Overlay::Static(_) => false,
        }
    }
}

fn first_or_empty(bindings: &[KeyBinding]) -> Vec<KeyBinding> {
    bindings.first().copied().into_iter().collect()
}

// Render a single line of key hints from (key(s), description) pairs.
fn render_key_hints(area: Rect, buf: &mut Buffer, pairs: &[(Vec<KeyBinding>, &str)]) {
    let mut spans: Vec<Span<'static>> = vec![" ".into()];
    let mut first = true;
    for (keys, desc) in pairs {
        if !first {
            spans.push("   ".into());
        }
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                spans.push("/".into());
            }
            spans.push(Span::from(key));
        }
        spans.push(" ".into());
        spans.push(Span::from(desc.to_string()));
        first = false;
    }
    Paragraph::new(vec![Line::from(spans).dim()]).render_ref(area, buf);
}

/// Generic widget for rendering a pager view.
struct PagerView {
    renderables: Vec<Box<dyn Renderable>>,
    layout_width: Option<u16>,
    chunk_bottoms: Vec<usize>,
    dynamic_layout_revisions: Vec<(usize, u64)>,
    scroll_offset: usize,
    title: String,
    keymap: PagerKeymap,
    last_content_height: Option<usize>,
    last_rendered_height: Option<usize>,
    /// If set, on next render ensure this chunk is visible.
    pending_scroll_chunk: Option<usize>,
    pending_align_chunk_top: Option<usize>,
}

impl PagerView {
    fn new(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        scroll_offset: usize,
        keymap: PagerKeymap,
    ) -> Self {
        let dynamic_layout_revisions = Self::collect_dynamic_layout_revisions(&renderables);
        Self {
            renderables,
            layout_width: None,
            chunk_bottoms: Vec::new(),
            dynamic_layout_revisions,
            scroll_offset,
            title,
            keymap,
            last_content_height: None,
            last_rendered_height: None,
            pending_scroll_chunk: None,
            pending_align_chunk_top: None,
        }
    }

    fn collect_dynamic_layout_revisions(renderables: &[Box<dyn Renderable>]) -> Vec<(usize, u64)> {
        renderables
            .iter()
            .enumerate()
            .filter_map(|(index, renderable)| {
                renderable
                    .layout_revision()
                    .map(|revision| (index, revision))
            })
            .collect()
    }

    fn first_dynamic_layout_change(&mut self) -> Option<usize> {
        let mut first_changed = None;
        for (index, cached_revision) in &mut self.dynamic_layout_revisions {
            let Some(revision) = self.renderables[*index].layout_revision() else {
                continue;
            };
            if revision != *cached_revision {
                *cached_revision = revision;
                first_changed =
                    Some(first_changed.map_or(*index, |first: usize| first.min(*index)));
            }
        }
        first_changed
    }

    fn refresh_layout(&mut self, width: u16) {
        let rebuild_all =
            self.layout_width != Some(width) || self.chunk_bottoms.len() != self.renderables.len();
        let first_changed = self.first_dynamic_layout_change();
        if !rebuild_all && first_changed.is_none() {
            return;
        }

        let first_to_measure = if rebuild_all {
            self.chunk_bottoms.clear();
            self.chunk_bottoms.reserve(self.renderables.len());
            0
        } else if let Some(first_changed) = first_changed {
            self.chunk_bottoms.truncate(first_changed);
            first_changed
        } else {
            return;
        };
        let mut bottom = self.chunk_bottoms.last().copied().unwrap_or(0);
        for renderable in self.renderables.iter().skip(first_to_measure) {
            bottom = bottom.saturating_add(renderable.desired_height_usize(width));
            self.chunk_bottoms.push(bottom);
        }
        self.layout_width = Some(width);
    }

    fn invalidate_layout(&mut self) {
        self.layout_width = None;
        self.chunk_bottoms.clear();
    }

    fn push_renderable(&mut self, renderable: Box<dyn Renderable>) {
        let cached_bottom = self.chunk_bottoms.last().copied().unwrap_or(0);
        let cached_height = self
            .layout_width
            .map(|width| cached_bottom.saturating_add(renderable.desired_height_usize(width)));
        let layout_revision = renderable.layout_revision();
        let index = self.renderables.len();
        self.renderables.push(renderable);
        if let Some(layout_revision) = layout_revision {
            self.dynamic_layout_revisions.push((index, layout_revision));
        }
        if let Some(bottom) = cached_height {
            self.chunk_bottoms.push(bottom);
        } else {
            self.invalidate_layout();
        }
    }

    fn pop_renderable(&mut self) -> Option<Box<dyn Renderable>> {
        let renderable = self.renderables.pop();
        if renderable.is_some() {
            if self
                .dynamic_layout_revisions
                .last()
                .is_some_and(|(index, _)| *index >= self.renderables.len())
            {
                self.dynamic_layout_revisions.pop();
            }
            if self.layout_width.is_some() && self.chunk_bottoms.len() == self.renderables.len() + 1
            {
                self.chunk_bottoms.pop();
            } else {
                self.invalidate_layout();
            }
        }
        renderable
    }

    fn replace_renderables(&mut self, renderables: Vec<Box<dyn Renderable>>) {
        self.renderables = renderables;
        self.dynamic_layout_revisions = Self::collect_dynamic_layout_revisions(&self.renderables);
        self.pending_scroll_chunk = None;
        self.pending_align_chunk_top = None;
        self.invalidate_layout();
    }

    fn content_height(&self) -> usize {
        self.chunk_bottoms.last().copied().unwrap_or(0)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        self.render_header(area, buf);
        let content_area = self.content_area(area);
        self.update_last_content_height(content_area.height);
        self.refresh_layout(content_area.width);
        let content_height = self.content_height();
        self.last_rendered_height = Some(content_height);
        // If there is a pending request to scroll a specific chunk into view,
        // satisfy it now that wrapping is up to date for this width.
        if let Some(idx) = self.pending_scroll_chunk.take() {
            self.ensure_chunk_visible(idx, content_area);
        }
        if let Some(idx) = self.pending_align_chunk_top.take()
            && idx < self.chunk_bottoms.len()
        {
            self.scroll_offset = idx
                .checked_sub(1)
                .map_or(0, |previous| self.chunk_bottoms[previous]);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(content_height.saturating_sub(content_area.height as usize));

        self.render_content(content_area, buf);

        self.render_bottom_bar(area, content_area, buf, content_height);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        Span::from("/ ".repeat(area.width as usize / 2))
            .dim()
            .render_ref(area, buf);
        let header = format!("/ {}", self.title);
        header.dim().render_ref(area, buf);
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        let first_visible = self
            .chunk_bottoms
            .partition_point(|bottom| *bottom <= self.scroll_offset);
        let mut top = if first_visible == 0 {
            0
        } else {
            self.chunk_bottoms[first_visible - 1]
        };
        let mut drawn_bottom = area.y;
        for (index, renderable) in self.renderables.iter().enumerate().skip(first_visible) {
            let bottom = self.chunk_bottoms[index];
            let height = bottom.saturating_sub(top);
            let visible_top = top.saturating_sub(self.scroll_offset);
            if visible_top >= usize::from(area.height) {
                break;
            }
            if top < self.scroll_offset {
                let scroll_rows = self.scroll_offset - top;
                let drawn = height
                    .saturating_sub(scroll_rows)
                    .min(usize::from(area.height));
                let drawn = u16::try_from(drawn).unwrap_or(area.height);
                let draw_area = Rect::new(area.x, area.y, area.width, drawn);
                renderable.render_with_offset(draw_area, buf, scroll_rows);
                drawn_bottom = drawn_bottom.max(area.y + drawn);
            } else {
                let draw_height = u16::try_from(height)
                    .unwrap_or(u16::MAX)
                    .min(area.height.saturating_sub(visible_top as u16));
                let draw_area =
                    Rect::new(area.x, area.y + visible_top as u16, area.width, draw_height);
                renderable.render(draw_area, buf);
                drawn_bottom = drawn_bottom.max(draw_area.y.saturating_add(draw_area.height));
            }
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
        let sep_y = content_area.bottom();
        let sep_rect = Rect::new(full_area.x, sep_y, full_area.width, 1);

        Span::from("─".repeat(sep_rect.width as usize))
            .dim()
            .render_ref(sep_rect, buf);
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
        let Some(pct_offset) = sep_rect.width.checked_sub(pct_w.saturating_add(1)) else {
            return;
        };
        let pct_x = sep_rect.x.saturating_add(pct_offset);
        Span::from(pct_text)
            .dim()
            .render_ref(Rect::new(pct_x, sep_rect.y, pct_w, 1), buf);
    }

    fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) -> Result<()> {
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
                let area = self.content_area(tui.terminal.viewport_area);
                let half_page = (area.height as usize).saturating_add(1) / 2;
                self.scroll_offset = self.scroll_offset.saturating_add(half_page);
            }
            e if self.keymap.half_page_up.is_pressed(e) => {
                let area = self.content_area(tui.terminal.viewport_area);
                let half_page = (area.height as usize).saturating_add(1) / 2;
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
        self.pending_scroll_chunk = None;
        self.pending_align_chunk_top = None;
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

impl PagerView {
    fn is_scrolled_to_bottom(&self) -> bool {
        if self.scroll_offset == usize::MAX {
            return true;
        }
        let Some(height) = self.last_content_height else {
            return false;
        };
        if self.renderables.is_empty() {
            return true;
        }
        let Some(total_height) = self.last_rendered_height else {
            return false;
        };
        if total_height <= height {
            return true;
        }
        let max_scroll = total_height.saturating_sub(height);
        self.scroll_offset >= max_scroll
    }

    /// Request that the given text chunk index be scrolled into view on next render.
    fn scroll_chunk_into_view(&mut self, chunk_index: usize) {
        self.pending_align_chunk_top = None;
        self.pending_scroll_chunk = Some(chunk_index);
    }

    fn align_chunk_to_top(&mut self, chunk_index: usize) {
        self.pending_scroll_chunk = None;
        self.pending_align_chunk_top = Some(chunk_index);
    }

    fn first_visible_chunk(&self) -> usize {
        self.chunk_bottoms
            .partition_point(|bottom| *bottom <= self.scroll_offset)
            .min(self.renderables.len().saturating_sub(1))
    }

    fn first_chunk_starting_at_or_below_top(&self) -> usize {
        let first_visible = self.first_visible_chunk();
        let chunk_top = first_visible
            .checked_sub(1)
            .and_then(|index| self.chunk_bottoms.get(index))
            .copied()
            .unwrap_or(0);
        first_visible + usize::from(chunk_top < self.scroll_offset)
    }

    fn is_scroll_key(&self, key_event: KeyEvent) -> bool {
        self.keymap.scroll_up.is_pressed(key_event)
            || self.keymap.scroll_down.is_pressed(key_event)
            || self.keymap.page_up.is_pressed(key_event)
            || self.keymap.page_down.is_pressed(key_event)
            || self.keymap.half_page_up.is_pressed(key_event)
            || self.keymap.half_page_down.is_pressed(key_event)
            || self.keymap.jump_top.is_pressed(key_event)
            || self.keymap.jump_bottom.is_pressed(key_event)
    }

    fn ensure_chunk_visible(&mut self, idx: usize, area: Rect) {
        if area.height == 0 || idx >= self.renderables.len() {
            return;
        }
        self.refresh_layout(area.width);
        let first = idx
            .checked_sub(1)
            .map_or(0, |previous| self.chunk_bottoms[previous]);
        let last = self.chunk_bottoms[idx];
        let current_top = self.scroll_offset;
        let current_bottom = current_top.saturating_add(usize::from(area.height));
        if first < current_top {
            self.scroll_offset = first;
        } else if last > current_bottom {
            self.scroll_offset = last.saturating_sub(usize::from(area.height));
        }
    }
}

/// A renderable that caches its desired height.
struct CachedRenderable {
    renderable: Box<dyn Renderable>,
    height: std::cell::Cell<Option<usize>>,
    last_width: std::cell::Cell<Option<u16>>,
    last_layout_revision: std::cell::Cell<Option<u64>>,
}

impl CachedRenderable {
    fn new(renderable: impl Into<Box<dyn Renderable>>) -> Self {
        Self {
            renderable: renderable.into(),
            height: std::cell::Cell::new(None),
            last_width: std::cell::Cell::new(None),
            last_layout_revision: std::cell::Cell::new(None),
        }
    }
}

impl Renderable for CachedRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.renderable.render(area, buf);
    }
    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        self.renderable.render_with_offset(area, buf, scroll_offset);
    }
    fn layout_revision(&self) -> Option<u64> {
        self.renderable.layout_revision()
    }
    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.desired_height_usize(width)).unwrap_or(u16::MAX)
    }
    fn desired_height_usize(&self, width: u16) -> usize {
        let layout_revision = self.renderable.layout_revision();
        if self.last_width.get() != Some(width)
            || self.last_layout_revision.get() != layout_revision
        {
            let height = self.renderable.desired_height_usize(width);
            self.height.set(Some(height));
            self.last_width.set(Some(width));
            self.last_layout_revision.set(layout_revision);
        }
        self.height.get().unwrap_or(0)
    }
}

struct CellRenderable {
    cell: Arc<dyn HistoryCell>,
    style: Style,
    detail_mode: TranscriptDetailMode,
    has_dynamic_layout: bool,
    cache: std::cell::RefCell<Option<CellRenderCache>>,
}

struct CellRenderCache {
    width: u16,
    animation_tick: Option<u64>,
    rows: RenderedHyperlinkLines,
}

struct RenderedHyperlinkLines {
    rows: Vec<HyperlinkLine>,
}

impl RenderedHyperlinkLines {
    fn new(lines: &[HyperlinkLine], width: u16, style: Style) -> Self {
        Self {
            rows: wrap_hyperlink_lines(lines, width, style),
        }
    }

    fn new_for_transcript_cell(lines: &[HyperlinkLine], width: u16, style: Style) -> Self {
        let mut rendered = Self::new(lines, width, style);
        if let [line] = lines
            && line
                .line
                .spans
                .iter()
                .all(|span| span.content.chars().all(char::is_whitespace))
        {
            // Preserve HistoryCell::desired_transcript_height's workaround for Ratatui
            // materializing a single whitespace-only logical line as two rows. This normalization
            // belongs at the transcript-cell boundary: static and multi-line pager content must
            // retain the wrapper's ordinary Ratatui-compatible row semantics.
            rendered.rows.truncate(1);
        }
        rendered
    }

    fn viewport(&self, scroll_offset: usize, height: u16) -> &[HyperlinkLine] {
        let start = scroll_offset.min(self.rows.len());
        let end = start
            .saturating_add(usize::from(height))
            .min(self.rows.len());
        &self.rows[start..end]
    }

    fn height(&self) -> usize {
        self.rows.len()
    }
}

impl CellRenderable {
    fn new(cell: Arc<dyn HistoryCell>, style: Style, detail_mode: TranscriptDetailMode) -> Self {
        let has_dynamic_layout = cell.transcript_animation_tick().is_some();
        Self {
            cell,
            style,
            detail_mode,
            has_dynamic_layout,
            cache: std::cell::RefCell::new(None),
        }
    }

    fn with_render_cache<T>(&self, width: u16, f: impl FnOnce(&CellRenderCache) -> T) -> T {
        let animation_tick = self.cell.transcript_animation_tick();
        let mut cache = self.cache.borrow_mut();
        if cache
            .as_ref()
            .is_some_and(|cache| cache.width != width || cache.animation_tick != animation_tick)
        {
            cache.take();
        }
        let cache = cache.get_or_insert_with(|| {
            let hyperlink_lines = match self.detail_mode {
                TranscriptDetailMode::Review => self.cell.display_hyperlink_lines(width),
                TranscriptDetailMode::Full => self.cell.transcript_hyperlink_lines(width),
            };
            let rows = RenderedHyperlinkLines::new_for_transcript_cell(
                &hyperlink_lines,
                width,
                self.style,
            );
            CellRenderCache {
                width,
                animation_tick,
                rows,
            }
        });
        f(cache)
    }
}

impl Renderable for CellRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.with_render_cache(area.width, |cache| {
            let rows = cache.rows.viewport(/*scroll_offset*/ 0, area.height);
            Paragraph::new(Text::from(visible_lines(rows.to_vec())))
                .style(self.style)
                .render(area, buf);
            mark_buffer_hyperlinks_in_rows(buf, area, rows);
        });
    }

    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        self.with_render_cache(area.width, |cache| {
            let rows = cache.rows.viewport(scroll_offset, area.height);
            Paragraph::new(Text::from(visible_lines(rows.to_vec())))
                .style(self.style)
                .render(area, buf);
            mark_buffer_hyperlinks_in_rows(buf, area, rows);
        });
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.desired_height_usize(width)).unwrap_or(u16::MAX)
    }

    fn desired_height_usize(&self, width: u16) -> usize {
        self.with_render_cache(width, |cache| cache.rows.height())
    }

    fn layout_revision(&self) -> Option<u64> {
        self.has_dynamic_layout.then(|| {
            self.cell
                .transcript_animation_tick()
                .map_or(0, |tick| tick.wrapping_add(1))
        })
    }
}

struct HyperlinkLinesRenderable {
    lines: Vec<HyperlinkLine>,
    cache: std::cell::RefCell<Option<(u16, RenderedHyperlinkLines)>>,
}

impl HyperlinkLinesRenderable {
    fn new(lines: Vec<HyperlinkLine>) -> Self {
        Self {
            lines,
            cache: std::cell::RefCell::new(None),
        }
    }

    fn with_render_cache<T>(&self, width: u16, f: impl FnOnce(&RenderedHyperlinkLines) -> T) -> T {
        let mut cache = self.cache.borrow_mut();
        if cache
            .as_ref()
            .is_some_and(|(cached_width, _)| *cached_width != width)
        {
            cache.take();
        }
        let cache = cache.get_or_insert_with(|| {
            (
                width,
                RenderedHyperlinkLines::new(&self.lines, width, Style::default()),
            )
        });
        f(&cache.1)
    }
}

impl Renderable for HyperlinkLinesRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.with_render_cache(area.width, |cache| {
            let rows = cache.viewport(/*scroll_offset*/ 0, area.height);
            Paragraph::new(Text::from(visible_lines(rows.to_vec()))).render(area, buf);
            mark_buffer_hyperlinks_in_rows(buf, area, rows);
        });
    }

    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        self.with_render_cache(area.width, |cache| {
            let rows = cache.viewport(scroll_offset, area.height);
            Paragraph::new(Text::from(visible_lines(rows.to_vec()))).render(area, buf);
            mark_buffer_hyperlinks_in_rows(buf, area, rows);
        });
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.desired_height_usize(width)).unwrap_or(u16::MAX)
    }

    fn desired_height_usize(&self, width: u16) -> usize {
        self.with_render_cache(width, RenderedHyperlinkLines::height)
    }
}

pub(crate) struct TranscriptOverlay {
    /// Pager UI state and the renderables currently displayed.
    ///
    /// The invariant is that `view.renderables` is `render_cells(cells)` plus an optional trailing
    /// live-tail renderable appended after the committed cells.
    view: PagerView,
    /// Committed transcript cells (does not include the live tail).
    cells: Vec<Arc<dyn HistoryCell>>,
    browser: TranscriptBrowserState,
    highlight_cell: Option<usize>,
    highlight_draw_pending: bool,
    /// Cache key for the render-only live tail appended after committed cells.
    live_tail_key: Option<LiveTailKey>,
    is_done: bool,
}

/// Cache key for the active-cell "live tail" appended to the transcript overlay.
///
/// Changing any field implies a different rendered tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveTailKey {
    /// Current terminal width, which affects wrapping.
    width: u16,
    /// Revision that changes on in-place active cell transcript updates.
    revision: u64,
    /// Whether the tail should be treated as a continuation for spacing.
    is_stream_continuation: bool,
    /// Optional animation tick to refresh spinners/progress indicators.
    animation_tick: Option<u64>,
    detail_mode: TranscriptDetailMode,
}

impl TranscriptOverlay {
    /// Creates a transcript overlay for a fixed set of committed cells.
    ///
    /// This overlay does not own the "active cell"; callers may optionally append a live tail via
    /// `sync_live_tail` during draws to reflect in-flight activity.
    fn new(
        transcript_cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
        flavor: TranscriptFlavor,
    ) -> Self {
        let browser = TranscriptBrowserState::new(flavor);
        Self {
            view: PagerView::new(
                Self::render_cells(
                    &transcript_cells,
                    /*highlight_cell*/ None,
                    browser.detail_mode(),
                ),
                transcript_title(browser),
                usize::MAX,
                keymap,
            ),
            cells: transcript_cells,
            browser,
            highlight_cell: None,
            highlight_draw_pending: false,
            live_tail_key: None,
            is_done: false,
        }
    }

    fn render_cells(
        cells: &[Arc<dyn HistoryCell>],
        highlight_cell: Option<usize>,
        detail_mode: TranscriptDetailMode,
    ) -> Vec<Box<dyn Renderable>> {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| Self::render_cell(cell, index, highlight_cell, detail_mode))
            .collect()
    }

    fn render_cell(
        cell: &Arc<dyn HistoryCell>,
        index: usize,
        highlight_cell: Option<usize>,
        detail_mode: TranscriptDetailMode,
    ) -> Box<dyn Renderable> {
        let mut cell_renderable = if cell.as_any().is::<UserHistoryCell>() {
            Box::new(CachedRenderable::new(CellRenderable::new(
                cell.clone(),
                if highlight_cell == Some(index) {
                    user_message_style().reversed()
                } else {
                    user_message_style()
                },
                detail_mode,
            ))) as Box<dyn Renderable>
        } else {
            Box::new(CachedRenderable::new(CellRenderable::new(
                cell.clone(),
                Style::default(),
                detail_mode,
            ))) as Box<dyn Renderable>
        };
        if !cell.is_stream_continuation() && index > 0 {
            cell_renderable = Box::new(InsetRenderable::new(
                cell_renderable,
                Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                ),
            ));
        }
        cell_renderable
    }

    /// Insert a committed history cell while keeping any cached live tail.
    ///
    /// The live tail is temporarily removed, one committed renderable is appended,
    /// then the tail is reattached. If the tail previously had no leading
    /// spacing because it was the only renderable, we add the missing inset
    /// when the first committed cell arrives.
    ///
    /// This expects `cell` to be a committed transcript cell (not the in-flight active cell). If
    /// the overlay was scrolled to bottom before insertion, it remains pinned to bottom after the
    /// insertion to preserve the "follow along" behavior.
    pub(crate) fn insert_cell(&mut self, cell: Arc<dyn HistoryCell>) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        let had_prior_cells = !self.cells.is_empty();
        let tail_renderable = self.take_live_tail_renderable();
        let index = self.cells.len();
        let cell_renderable = Self::render_cell(
            &cell,
            index,
            self.highlight_cell,
            self.browser.detail_mode(),
        );
        self.cells.push(cell);
        self.view.push_renderable(cell_renderable);
        if let Some(tail) = tail_renderable {
            let tail = if !had_prior_cells
                && self
                    .live_tail_key
                    .is_some_and(|key| !key.is_stream_continuation)
            {
                // The tail was rendered as the only entry, so it lacks a top
                // inset; add one now that it follows a committed cell.
                Box::new(InsetRenderable::new(
                    tail,
                    Insets::tlbr(
                        /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                    ),
                )) as Box<dyn Renderable>
            } else {
                tail
            };
            self.view.push_renderable(tail);
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Replace committed transcript cells while keeping any cached in-progress output that is
    /// currently shown at the end of the overlay.
    ///
    /// This is used when existing history is trimmed (for example after rollback) so the
    /// transcript overlay immediately reflects the same committed cells as the main transcript.
    pub(crate) fn replace_cells(&mut self, cells: Vec<Arc<dyn HistoryCell>>) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        self.cells = cells;
        self.browser.clear_review_target();
        if self
            .highlight_cell
            .is_some_and(|idx| idx >= self.cells.len())
        {
            self.highlight_cell = None;
        }
        self.rebuild_renderables();
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Replace a range of committed cells with a single consolidated cell.
    ///
    /// Mirrors the splice performed on `App::transcript_cells` during
    /// `ConsolidateAgentMessage` so the Ctrl+T overlay stays in sync with the
    /// main transcript. The range is clamped defensively: cells may have been
    /// inserted after the overlay opened, leaving it with fewer entries than
    /// the main transcript.
    pub(crate) fn consolidate_cells(
        &mut self,
        range: std::ops::Range<usize>,
        consolidated: Arc<dyn HistoryCell>,
    ) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        // Clamp the range to the overlay's cell count to avoid panic if the overlay has fewer
        // cells than the main transcript (e.g. cells were inserted after the overlay has opened).
        let clamped_end = range.end.min(self.cells.len());
        let clamped_start = range.start.min(clamped_end);
        if clamped_start < clamped_end {
            self.browser.consolidate(
                clamped_start,
                clamped_end,
                consolidated.transcript_navigation_kind().is_some(),
            );
            let removed = clamped_end - clamped_start;
            if let Some(highlight_cell) = self.highlight_cell.as_mut()
                && *highlight_cell >= clamped_start
            {
                if *highlight_cell < clamped_end {
                    *highlight_cell = clamped_start;
                } else {
                    *highlight_cell = highlight_cell.saturating_sub(removed.saturating_sub(1));
                }
            }
            self.cells
                .splice(clamped_start..clamped_end, std::iter::once(consolidated));
            if self
                .highlight_cell
                .is_some_and(|highlight_cell| highlight_cell >= self.cells.len())
            {
                self.highlight_cell = None;
            }
            self.rebuild_renderables();
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Sync the active-cell live tail with the current width and cell state.
    ///
    /// Recomputes the tail only when the cache key changes, preserving scroll
    /// position and dropping the tail if there is nothing to render.
    ///
    /// The overlay owns committed transcript cells while the live tail is derived from the current
    /// active cell, which can mutate in place while streaming. `App` calls this during
    /// `TuiEvent::Draw` for `Overlay::Transcript`, passing a key that changes when the active cell
    /// mutates or animates so the cached tail stays fresh.
    ///
    /// Passing a key that does not change on in-place active-cell mutations will freeze the tail in
    /// `Ctrl+T` while the main viewport continues to update.
    pub(crate) fn sync_live_tail(
        &mut self,
        width: u16,
        active_key: Option<ActiveCellTranscriptKey>,
        compute_lines: impl FnOnce(u16) -> Option<Vec<HyperlinkLine>>,
    ) {
        let next_key = active_key.map(|key| LiveTailKey {
            width,
            revision: key.revision,
            is_stream_continuation: key.is_stream_continuation,
            animation_tick: key.animation_tick,
            detail_mode: self.browser.detail_mode(),
        });

        if self.live_tail_key == next_key {
            return;
        }
        let follow_bottom = self.view.is_scrolled_to_bottom();

        self.take_live_tail_renderable();
        self.live_tail_key = next_key;

        if let Some(key) = next_key {
            let lines = compute_lines(width).unwrap_or_default();
            if !lines.is_empty() {
                self.view.push_renderable(Self::live_tail_renderable(
                    lines,
                    !self.cells.is_empty(),
                    key.is_stream_continuation,
                ));
            }
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    pub(crate) fn set_highlight_cell(&mut self, cell: Option<usize>) {
        let previous_highlight = self.highlight_cell;
        self.highlight_cell = cell;
        if previous_highlight != self.highlight_cell {
            self.highlight_draw_pending = true;
            for index in [previous_highlight, self.highlight_cell]
                .into_iter()
                .flatten()
            {
                if let Some(cell) = self.cells.get(index) {
                    // Highlighting only changes style, so the cached heights and chunk bottoms
                    // remain valid. Replacing just the affected cells preserves wrapping caches
                    // for the rest of the transcript, including other cells in the viewport.
                    self.view.renderables[index] = Self::render_cell(
                        cell,
                        index,
                        self.highlight_cell,
                        self.browser.detail_mode(),
                    );
                }
            }
        }
        if let Some(idx) = self.highlight_cell {
            self.view.scroll_chunk_into_view(idx);
        }
    }

    pub(crate) fn highlight_draw_pending(&self) -> bool {
        self.highlight_draw_pending
    }

    /// Returns whether the underlying pager view is currently pinned to the bottom.
    ///
    /// The `App` draw loop uses this to decide whether to schedule animation frames for the live
    /// tail; if the user has scrolled up, we avoid driving animation work that they cannot see.
    pub(crate) fn is_scrolled_to_bottom(&self) -> bool {
        self.view.is_scrolled_to_bottom()
    }

    fn rebuild_renderables(&mut self) {
        let tail_renderable = self.take_live_tail_renderable();
        self.view.replace_renderables(Self::render_cells(
            &self.cells,
            self.highlight_cell,
            self.browser.detail_mode(),
        ));
        if let Some(tail) = tail_renderable {
            self.view.push_renderable(tail);
        }
    }

    /// Removes and returns the cached live-tail renderable, if present.
    ///
    /// The live tail is represented as a single optional renderable appended after the committed
    /// cell renderables, so this relies on the live tail always being the final entry in
    /// `view.renderables` when present.
    fn take_live_tail_renderable(&mut self) -> Option<Box<dyn Renderable>> {
        (self.view.renderables.len() > self.cells.len())
            .then(|| self.view.pop_renderable())
            .flatten()
    }

    fn live_tail_renderable(
        lines: Vec<HyperlinkLine>,
        has_prior_cells: bool,
        is_stream_continuation: bool,
    ) -> Box<dyn Renderable> {
        let mut renderable: Box<dyn Renderable> =
            Box::new(CachedRenderable::new(HyperlinkLinesRenderable::new(lines)));
        if has_prior_cells && !is_stream_continuation {
            renderable = Box::new(InsetRenderable::new(
                renderable,
                Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                ),
            ));
        }
        renderable
    }
}

pub(crate) struct StaticOverlay {
    view: PagerView,
    is_done: bool,
}

impl StaticOverlay {
    pub(crate) fn with_title(
        lines: Vec<Line<'static>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::with_renderables(
            vec![Box::new(CachedRenderable::new(
                HyperlinkLinesRenderable::new(lines.into_iter().map(HyperlinkLine::from).collect()),
            ))],
            title,
            keymap,
        )
    }

    pub(crate) fn with_renderables(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self {
            view: PagerView::new(renderables, title, /*scroll_offset*/ 0, keymap),
            is_done: false,
        }
    }

    fn render_hints(&self, area: Rect, buf: &mut Buffer) {
        let line1 = Rect::new(area.x, area.y, area.width, 1);
        let line2 = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
        render_key_hints(
            line1,
            buf,
            &[
                (
                    first_or_empty(&self.view.keymap.scroll_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.scroll_down))
                        .collect(),
                    "to scroll",
                ),
                (
                    first_or_empty(&self.view.keymap.page_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.page_down))
                        .collect(),
                    "to page",
                ),
                (
                    first_or_empty(&self.view.keymap.jump_top)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.jump_bottom))
                        .collect(),
                    "to jump",
                ),
            ],
        );
        let pairs: Vec<(Vec<KeyBinding>, &str)> =
            vec![(first_or_empty(&self.view.keymap.close), "to quit")];
        render_key_hints(line2, buf, &pairs);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 3);
        self.view.render(top, buf);
        self.render_hints(bottom, buf);
    }
}

impl StaticOverlay {
    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if self.view.keymap.close.is_pressed(e) => {
                    self.is_done = true;
                    Ok(())
                }
                other => self.view.handle_key_event(tui, other),
            },
            TuiEvent::Draw | TuiEvent::Resize => {
                tui.draw(u16::MAX, |frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::ReviewDecision;
    use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use ratatui::widgets::Block;
    use ratatui::widgets::Borders;
    use std::cell::Cell as StdCell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::diff_model::FileChange;
    use crate::exec_cell::CommandOutput;
    use crate::history_cell;
    use crate::history_cell::HistoryCell;
    use crate::history_cell::new_patch_event;
    use codex_protocol::parse_command::ParsedCommand;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Text;

    #[derive(Debug)]
    struct TestCell {
        lines: Vec<Line<'static>>,
    }

    impl crate::history_cell::HistoryCell for TestCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }
    }

    #[derive(Debug)]
    struct CountingCell {
        calls: Arc<AtomicUsize>,
        lines: Vec<Line<'static>>,
        is_stream_continuation: bool,
    }

    impl crate::history_cell::HistoryCell for CountingCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.lines.clone()
        }

        fn is_stream_continuation(&self) -> bool {
            self.is_stream_continuation
        }
    }

    #[derive(Debug)]
    struct AnimatedCountingCell {
        calls: Arc<AtomicUsize>,
        animation_tick: Arc<AtomicUsize>,
    }

    impl crate::history_cell::HistoryCell for AnimatedCountingCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.transcript_lines(/*width*/ 0)
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.transcript_lines(/*width*/ 0)
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            vec![Line::from(format!(
                "frame-{}",
                self.animation_tick.load(Ordering::Relaxed)
            ))]
        }

        fn transcript_animation_tick(&self) -> Option<u64> {
            Some(self.animation_tick.load(Ordering::Relaxed) as u64)
        }
    }

    #[derive(Debug)]
    struct HeightChangingAnimatedCell {
        calls: Arc<AtomicUsize>,
        animation_tick: Arc<AtomicUsize>,
    }

    impl crate::history_cell::HistoryCell for HeightChangingAnimatedCell {
        fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
            self.transcript_lines(width)
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.transcript_lines(/*width*/ 0)
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let line_count = self.animation_tick.load(Ordering::Relaxed) + 1;
            (0..line_count)
                .map(|index| Line::from(format!("animated-{index}")))
                .collect()
        }

        fn transcript_animation_tick(&self) -> Option<u64> {
            Some(self.animation_tick.load(Ordering::Relaxed) as u64)
        }
    }

    fn paragraph_block(label: &str, lines: usize) -> Box<dyn Renderable> {
        let text = Text::from(
            (0..lines)
                .map(|i| Line::from(format!("{label}{i}")))
                .collect::<Vec<_>>(),
        );
        Box::new(Paragraph::new(text)) as Box<dyn Renderable>
    }

    struct CountingRenderable {
        height: u16,
        height_calls: Rc<StdCell<usize>>,
        revision_calls: Rc<StdCell<usize>>,
        rendered_offsets: Rc<std::cell::RefCell<Vec<usize>>>,
    }

    impl Renderable for CountingRenderable {
        fn render(&self, area: Rect, buf: &mut Buffer) {
            buf.set_string(area.x, area.y, "unscrolled", Style::default());
        }

        fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
            self.rendered_offsets.borrow_mut().push(scroll_offset);
            buf.set_string(
                area.x,
                area.y,
                format!("offset-{scroll_offset}"),
                Style::default(),
            );
        }

        fn desired_height(&self, _width: u16) -> u16 {
            self.height_calls.set(self.height_calls.get() + 1);
            self.height
        }

        fn layout_revision(&self) -> Option<u64> {
            self.revision_calls.set(self.revision_calls.get() + 1);
            None
        }
    }

    struct PaintAreaRenderable {
        height: u16,
        color: Color,
    }

    impl Renderable for PaintAreaRenderable {
        fn render(&self, area: Rect, buf: &mut Buffer) {
            buf.set_style(area, Style::default().bg(self.color));
        }

        fn render_with_offset(&self, area: Rect, buf: &mut Buffer, _scroll_offset: usize) {
            self.render(area, buf);
        }

        fn desired_height(&self, _width: u16) -> u16 {
            self.height
        }
    }

    struct MaxHeightRenderable;

    impl Renderable for MaxHeightRenderable {
        fn render(&self, _area: Rect, _buf: &mut Buffer) {}

        fn desired_height(&self, _width: u16) -> u16 {
            u16::MAX
        }
    }

    fn default_pager_keymap() -> crate::keymap::PagerKeymap {
        crate::keymap::RuntimeKeymap::defaults().pager
    }

    fn transcript_overlay(cells: Vec<Arc<dyn HistoryCell>>) -> TranscriptOverlay {
        TranscriptOverlay::new(
            cells,
            default_pager_keymap(),
            TranscriptFlavor::HistoricalFullPreview,
        )
    }

    fn static_overlay(lines: Vec<Line<'static>>, title: &str) -> StaticOverlay {
        StaticOverlay::with_title(lines, title.to_string(), default_pager_keymap())
    }

    fn pager_view(
        renderables: Vec<Box<dyn Renderable>>,
        title: &str,
        scroll_offset: usize,
    ) -> PagerView {
        PagerView::new(
            renderables,
            title.to_string(),
            scroll_offset,
            default_pager_keymap(),
        )
    }

    #[test]
    fn next_navigation_anchor_skips_chunk_containing_viewport_top() {
        let mut view = pager_view(
            vec![paragraph_block("long-", 3), paragraph_block("next-", 1)],
            "T",
            /*scroll_offset*/ 1,
        );
        view.refresh_layout(/*width*/ 40);

        assert_eq!(view.first_visible_chunk(), 0);
        assert_eq!(view.first_chunk_starting_at_or_below_top(), 1);
    }

    #[test]
    fn highlight_scroll_cancels_pending_review_alignment() {
        let mut view = pager_view(
            vec![paragraph_block("cell-", 1)],
            "T",
            /*scroll_offset*/ 0,
        );
        view.align_chunk_to_top(0);

        view.scroll_chunk_into_view(0);

        assert_eq!(view.pending_align_chunk_top, None);
        assert_eq!(view.pending_scroll_chunk, Some(0));
    }

    #[test]
    fn edit_prev_hint_is_visible() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("hello")],
        })]);

        // Render into a wide buffer so the footer hints aren't truncated.
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let s = buffer_to_text(&buf, area);
        assert!(
            s.contains("edit prev"),
            "expected 'edit prev' hint in overlay footer, got: {s:?}"
        );
    }

    #[test]
    fn edit_next_hint_is_visible_when_highlighted() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("hello")],
        })]);
        overlay.set_highlight_cell(Some(0));

        // Render into a wide buffer so the footer hints aren't truncated.
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let s = buffer_to_text(&buf, area);
        assert!(
            s.contains("edit next"),
            "expected 'edit next' hint in overlay footer, got: {s:?}"
        );
    }

    #[test]
    fn transcript_overlay_snapshot_basic() {
        // Prepare a transcript overlay with a few lines
        let mut overlay = transcript_overlay(vec![
            Arc::new(TestCell {
                lines: vec![Line::from("alpha")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("beta")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("gamma")],
            }),
        ]);
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn transcript_overlay_caches_committed_cell_lines_for_width() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut overlay = transcript_overlay(vec![Arc::new(CountingCell {
            calls: Arc::clone(&calls),
            lines: vec![Line::from("cached")],
            is_stream_continuation: false,
        })]);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);
        overlay.render(area, &mut buf);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn transcript_overlay_highlight_navigation_preserves_neighboring_cell_caches() {
        let area = Rect::new(0, 0, 40, 24);
        let cell_count = usize::from(area.height) * 2;
        let calls = (0..cell_count)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect::<Vec<_>>();
        let mut overlay = transcript_overlay(
            calls
                .iter()
                .enumerate()
                .map(|(index, calls)| {
                    Arc::new(CountingCell {
                        calls: Arc::clone(calls),
                        lines: vec![Line::from(format!("cell-{index}"))],
                        is_stream_continuation: true,
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);
        let expected_layout_cache = (
            overlay.view.layout_width,
            overlay.view.chunk_bottoms.clone(),
            overlay.view.dynamic_layout_revisions.clone(),
        );
        overlay.set_highlight_cell(Some(16));
        assert_eq!(
            (
                overlay.view.layout_width,
                overlay.view.chunk_bottoms.clone(),
                overlay.view.dynamic_layout_revisions.clone(),
            ),
            expected_layout_cache
        );
        overlay.render(area, &mut buf);
        overlay.set_highlight_cell(Some(18));
        assert_eq!(
            (
                overlay.view.layout_width,
                overlay.view.chunk_bottoms.clone(),
                overlay.view.dynamic_layout_revisions.clone(),
            ),
            expected_layout_cache
        );
        overlay.render(area, &mut buf);

        let mut expected_calls = vec![1; cell_count];
        expected_calls[16] = 3;
        expected_calls[18] = 2;
        assert_eq!(
            calls
                .iter()
                .map(|calls| calls.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_calls
        );
        assert_eq!(
            buffer_to_text(&buf, area).matches("cell-").count(),
            overlay
                .view
                .last_content_height
                .expect("the overlay should record its content viewport height"),
            "short neighboring cells should fill the rendered viewport"
        );
    }

    #[test]
    fn transcript_overlay_highlight_navigation_transfers_reversed_style() {
        let user_cell = |message: &str| {
            Arc::new(UserHistoryCell {
                message: message.to_string(),
                text_elements: Vec::new(),
                local_image_paths: Vec::new(),
                remote_image_urls: Vec::new(),
            }) as Arc<dyn HistoryCell>
        };
        let mut overlay =
            transcript_overlay(vec![user_cell("first prompt"), user_cell("second prompt")]);
        let area = Rect::new(0, 0, 40, 16);
        let mut buf = Buffer::empty(area);
        let is_reversed = |buf: &Buffer, text: &str| {
            let chars = text.chars().collect::<Vec<_>>();
            area.positions().any(|position| {
                let (x, y) = (position.x, position.y);
                let remaining = usize::from(area.right().saturating_sub(x));
                remaining >= chars.len()
                    && chars.iter().enumerate().all(|(offset, expected)| {
                        buf[(x + offset as u16, y)].symbol().starts_with(*expected)
                    })
                    && buf[(x, y)].modifier.contains(Modifier::REVERSED)
            })
        };
        let highlight_state = |buf: &Buffer| {
            (
                is_reversed(buf, "first prompt"),
                is_reversed(buf, "second prompt"),
            )
        };

        overlay.render(area, &mut buf);
        assert_eq!(highlight_state(&buf), (false, false));

        overlay.set_highlight_cell(Some(0));
        overlay.render(area, &mut buf);
        assert_eq!(highlight_state(&buf), (true, false));

        overlay.set_highlight_cell(Some(1));
        overlay.render(area, &mut buf);
        assert_eq!(highlight_state(&buf), (false, true));

        overlay.set_highlight_cell(/*cell*/ None);
        overlay.render(area, &mut buf);
        assert_eq!(highlight_state(&buf), (false, false));
    }

    #[test]
    fn transcript_overlay_single_whitespace_only_cell_occupies_one_row() {
        for whitespace in ["", "  \t"] {
            let mut overlay = transcript_overlay(vec![
                Arc::new(TestCell {
                    lines: vec![Line::from(whitespace)],
                }),
                Arc::new(TestCell {
                    lines: vec![Line::from("following")],
                }),
            ]);
            let area = Rect::new(0, 0, 20, 4);
            let mut buf = Buffer::empty(area);

            overlay.view.refresh_layout(area.width);
            overlay.view.scroll_offset = 0;
            overlay.view.render_content(area, &mut buf);

            assert_eq!(
                overlay.view.chunk_bottoms,
                vec![1, 3],
                "single whitespace-only transcript cell {whitespace:?} should occupy one row"
            );
            assert_eq!(
                buf[(0, 2)].symbol(),
                "f",
                "the following chunk should start after the blank row and its normal top inset"
            );
        }
    }

    #[test]
    fn transcript_overlay_invalidates_animated_committed_cell_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let animation_tick = Arc::new(AtomicUsize::new(0));
        let mut overlay = transcript_overlay(vec![Arc::new(AnimatedCountingCell {
            calls: Arc::clone(&calls),
            animation_tick: Arc::clone(&animation_tick),
        })]);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);
        assert!(buffer_to_text(&buf, area).contains("frame-0"));

        animation_tick.store(1, Ordering::Relaxed);
        overlay.render(area, &mut buf);

        assert!(buffer_to_text(&buf, area).contains("frame-1"));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn transcript_overlay_invalidates_offscreen_animated_height_and_prefix_index() {
        let calls = Arc::new(AtomicUsize::new(0));
        let animation_tick = Arc::new(AtomicUsize::new(0));
        let mut overlay = transcript_overlay(vec![
            Arc::new(TestCell {
                lines: (0..20)
                    .map(|index| Line::from(format!("leading-{index}")))
                    .collect(),
            }),
            Arc::new(HeightChangingAnimatedCell {
                calls: Arc::clone(&calls),
                animation_tick: Arc::clone(&animation_tick),
            }),
        ]);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        assert_eq!(overlay.view.dynamic_layout_revisions.len(), 1);
        overlay.view.scroll_offset = 0;
        overlay.render(area, &mut buf);
        let initial_bottom = *overlay
            .view
            .chunk_bottoms
            .last()
            .expect("animated cell should have a prefix-index entry");

        animation_tick.store(3, Ordering::Relaxed);
        overlay.render(area, &mut buf);
        let updated_bottom = *overlay
            .view
            .chunk_bottoms
            .last()
            .expect("animated cell should retain a prefix-index entry");

        assert_eq!(updated_bottom, initial_bottom + 3);
        assert_eq!(overlay.view.last_rendered_height, Some(updated_bottom));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(
            !buffer_to_text(&buf, area).contains("animated-"),
            "the height-changing cell should remain offscreen during invalidation"
        );
    }

    #[test]
    fn transcript_overlay_preserves_semantic_web_links() {
        let destination = "https://example.com/a/very/long/path";
        let mut overlay = transcript_overlay(vec![Arc::new(history_cell::AgentMarkdownCell::new(
            destination.to_string(),
            std::path::Path::new("/tmp"),
        ))]);
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 24, /*height*/ 10,
        );
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);

        assert!(area.positions().any(|position| {
            buf[position]
                .symbol()
                .contains(&format!("\x1b]8;;{destination}\x07"))
        }));
    }

    #[test]
    fn transcript_overlay_renders_live_tail() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("alpha")],
        })]);
        overlay.sync_live_tail(
            /*width*/ 40,
            Some(ActiveCellTranscriptKey {
                revision: 1,
                is_stream_continuation: false,
                animation_tick: None,
            }),
            |_| Some(vec![HyperlinkLine::from("tail")]),
        );

        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn transcript_overlay_live_tail_preserves_semantic_web_links() {
        let destination = "https://example.com/a/streamed/path";
        let cell = history_cell::AgentMarkdownCell::new(
            destination.to_string(),
            std::path::Path::new("/tmp"),
        );
        let mut overlay = transcript_overlay(Vec::new());
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 24, /*height*/ 10,
        );
        let mut buf = Buffer::empty(area);

        overlay.sync_live_tail(
            area.width,
            Some(ActiveCellTranscriptKey {
                revision: 1,
                is_stream_continuation: false,
                animation_tick: None,
            }),
            |width| Some(cell.transcript_hyperlink_lines(width)),
        );
        overlay.render(area, &mut buf);

        assert!(area.positions().any(|position| {
            buf[position]
                .symbol()
                .contains(&format!("\x1b]8;;{destination}\x07"))
        }));
    }

    #[test]
    fn transcript_overlay_sync_live_tail_is_noop_for_identical_key() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("alpha")],
        })]);

        let calls = std::cell::Cell::new(0usize);
        let key = ActiveCellTranscriptKey {
            revision: 1,
            is_stream_continuation: false,
            animation_tick: None,
        };

        overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
            calls.set(calls.get() + 1);
            Some(vec![HyperlinkLine::from("tail")])
        });
        overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
            calls.set(calls.get() + 1);
            Some(vec![HyperlinkLine::from("tail2")])
        });

        assert_eq!(calls.get(), 1);
    }

    fn buffer_to_text(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let symbol = buf[(x, y)].symbol();
                if symbol.is_empty() {
                    out.push(' ');
                } else {
                    out.push(symbol.chars().next().unwrap_or(' '));
                }
            }
            // Trim trailing spaces for stability.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn transcript_overlay_apply_patch_scroll_vt100_clears_previous_page() {
        let cwd = PathBuf::from("/repo");
        let mut cells: Vec<Arc<dyn HistoryCell>> = Vec::new();

        let mut approval_changes = HashMap::new();
        approval_changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\nworld\n".to_string(),
            },
        );
        let approval_cell: Arc<dyn HistoryCell> = Arc::new(new_patch_event(approval_changes, &cwd));
        cells.push(approval_cell);

        let mut apply_changes = HashMap::new();
        apply_changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\nworld\n".to_string(),
            },
        );
        let apply_begin_cell: Arc<dyn HistoryCell> = Arc::new(new_patch_event(apply_changes, &cwd));
        cells.push(apply_begin_cell);

        let apply_end_cell: Arc<dyn HistoryCell> = history_cell::new_approval_decision_cell(
            history_cell::ApprovalDecisionSubject::Command(vec!["ls".into()]),
            ReviewDecision::Approved,
            history_cell::ApprovalDecisionActor::User,
        )
        .into();
        cells.push(apply_end_cell);

        let mut exec_cell = crate::exec_cell::new_active_exec_command(
            "exec-1".into(),
            vec!["bash".into(), "-lc".into(), "ls".into()],
            vec![ParsedCommand::Unknown { cmd: "ls".into() }],
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
            /*animations_enabled*/ true,
            crate::exec_cell::OutputPreviewLineLimits {
                command: codex_config::types::DEFAULT_TUI_COMMAND_OUTPUT_PREVIEW_LINES,
                user_shell: codex_config::types::DEFAULT_TUI_USER_SHELL_OUTPUT_PREVIEW_LINES,
            },
        );
        exec_cell.complete_call(
            "exec-1",
            CommandOutput {
                exit_code: 0,
                aggregated_output: "src\nREADME.md\n".into(),
                formatted_output: "src\nREADME.md\n".into(),
            },
            Duration::from_millis(420),
        );
        let exec_cell: Arc<dyn HistoryCell> = Arc::new(exec_cell);
        cells.push(exec_cell);

        let mut overlay = transcript_overlay(cells);
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);
        overlay.view.scroll_offset = 0;
        overlay.render(area, &mut buf);

        let snapshot = buffer_to_text(&buf, area);
        assert_snapshot!("transcript_overlay_apply_patch_scroll_vt100", snapshot);
    }

    #[test]
    fn transcript_overlay_keeps_scroll_pinned_at_bottom() {
        let mut overlay = transcript_overlay(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        assert!(
            overlay.view.is_scrolled_to_bottom(),
            "expected initial render to leave view at bottom"
        );

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, usize::MAX);
    }

    #[test]
    fn transcript_overlay_preserves_manual_scroll_position() {
        let mut overlay = transcript_overlay(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        overlay.view.scroll_offset = 0;

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, 0);
    }

    #[test]
    fn transcript_overlay_consolidation_remaps_highlight_inside_range() {
        let mut overlay = transcript_overlay(
            (0..6)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        overlay.set_highlight_cell(Some(3));

        overlay.consolidate_cells(
            2..5,
            Arc::new(TestCell {
                lines: vec![Line::from("consolidated")],
            }),
        );

        assert_eq!(
            overlay.highlight_cell,
            Some(2),
            "highlight inside consolidated range should point to replacement cell",
        );
    }

    #[test]
    fn transcript_overlay_consolidation_remaps_highlight_after_range() {
        let mut overlay = transcript_overlay(
            (0..7)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        overlay.set_highlight_cell(Some(6));

        overlay.consolidate_cells(
            2..5,
            Arc::new(TestCell {
                lines: vec![Line::from("consolidated")],
            }),
        );

        assert_eq!(
            overlay.highlight_cell,
            Some(4),
            "highlight after consolidated range should shift left by removed cells",
        );
    }

    #[test]
    fn static_overlay_snapshot_basic() {
        // Prepare a static overlay with a few lines and a title
        let mut overlay = static_overlay(
            vec!["one".into(), "two".into(), "three".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    /// Render transcript overlay and return visible line numbers (`line-NN`) in order.
    fn transcript_line_numbers(overlay: &mut TranscriptOverlay, area: Rect) -> Vec<usize> {
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let content_area = overlay.view.content_area(top);

        let mut nums = Vec::new();
        for y in content_area.y..content_area.bottom() {
            let mut line = String::new();
            for x in content_area.x..content_area.right() {
                line.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            if let Some(n) = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("line-"))
                .and_then(|s| s.parse().ok())
            {
                nums.push(n);
            }
        }
        nums
    }

    #[test]
    fn transcript_overlay_paging_is_continuous_and_round_trips() {
        let mut overlay = transcript_overlay(
            (0..50)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line-{i:02}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let area = Rect::new(0, 0, 40, 15);

        // Prime layout so last_content_height is populated and paging uses the real content height.
        let mut buf = Buffer::empty(area);
        overlay.view.scroll_offset = 0;
        overlay.render(area, &mut buf);
        let page_height = overlay.view.page_height(area);

        // Scenario 1: starting from the top, PageDown should show the next page of content.
        overlay.view.scroll_offset = 0;
        let page1 = transcript_line_numbers(&mut overlay, area);
        let page1_len = page1.len();
        let expected_page1: Vec<usize> = (0..page1_len).collect();
        assert_eq!(
            page1, expected_page1,
            "first page should start at line-00 and show a full page of content"
        );

        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let page2 = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            page2.len(),
            page1_len,
            "second page should have the same number of visible lines as the first page"
        );
        let expected_page2_first = *page1.last().unwrap() + 1;
        assert_eq!(
            page2[0], expected_page2_first,
            "second page after PageDown should immediately follow the first page"
        );

        // Scenario 2: from an interior offset (start=3), PageDown then PageUp should round-trip.
        let interior_offset = 3usize;
        overlay.view.scroll_offset = interior_offset;
        let before = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let _ = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_sub(page_height);
        let after = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            before, after,
            "PageDown+PageUp from interior offset ({interior_offset}) should round-trip"
        );

        // Scenario 3: from the top of the second page, PageUp then PageDown should round-trip.
        overlay.view.scroll_offset = page_height;
        let before2 = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_sub(page_height);
        let _ = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let after2 = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            before2, after2,
            "PageUp+PageDown from the top of the second page should round-trip"
        );
    }

    #[test]
    fn static_overlay_wraps_long_lines() {
        let mut overlay = static_overlay(
            vec!["a very long line that should wrap when rendered within a narrow pager overlay width".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(24, 8)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn pager_view_content_height_counts_renderables() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 2),
                paragraph_block("b", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );

        pv.refresh_layout(/*width*/ 80);
        assert_eq!(pv.content_height(), 5);
    }

    #[test]
    fn pager_view_delegates_partial_cell_scrolling_without_a_tall_buffer() {
        let rendered_offsets = Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut pv = pager_view(
            vec![Box::new(CountingRenderable {
                height: 200,
                height_calls: Rc::new(StdCell::new(0)),
                revision_calls: Rc::new(StdCell::new(0)),
                rendered_offsets: Rc::clone(&rendered_offsets),
            })],
            "T",
            /*scroll_offset*/ 120,
        );
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        pv.render(area, &mut buf);

        assert_eq!(&*rendered_offsets.borrow(), &[120]);
        assert!(buffer_to_text(&buf, area).contains("offset-120"));
    }

    #[test]
    fn pager_view_bounds_partial_chunk_paint_before_following_inset() {
        let following = InsetRenderable::new(
            Box::new(Paragraph::new("following").style(Style::default().bg(Color::Green)))
                as Box<dyn Renderable>,
            Insets::tlbr(
                /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
            ),
        );
        let mut pv = pager_view(
            vec![
                Box::new(PaintAreaRenderable {
                    height: 3,
                    color: Color::Red,
                }),
                Box::new(following),
            ],
            "T",
            /*scroll_offset*/ 1,
        );
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        buf.set_style(area, Style::default().bg(Color::Blue));
        pv.refresh_layout(area.width);

        pv.render_content(area, &mut buf);

        assert_eq!(buf[(0, 0)].bg, Color::Red);
        assert_eq!(buf[(0, 1)].bg, Color::Red);
        assert_eq!(
            buf[(0, 2)].bg,
            Color::Blue,
            "the following chunk's top inset must not inherit paint from the partial chunk"
        );
        assert_eq!(buf[(0, 3)].symbol(), "f");
        assert_eq!(buf[(0, 3)].bg, Color::Green);
    }

    #[test]
    fn pager_view_does_not_poll_or_remeasure_stable_renderables_on_repeated_frames() {
        let height_calls = Rc::new(StdCell::new(0));
        let revision_calls = Rc::new(StdCell::new(0));
        let renderables = (0..100)
            .map(|_| {
                Box::new(CountingRenderable {
                    height: 1,
                    height_calls: Rc::clone(&height_calls),
                    revision_calls: Rc::clone(&revision_calls),
                    rendered_offsets: Rc::new(std::cell::RefCell::new(Vec::new())),
                }) as Box<dyn Renderable>
            })
            .collect();
        let mut pv = pager_view(renderables, "T", /*scroll_offset*/ 0);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        pv.render(area, &mut buf);
        let height_calls_after_layout = height_calls.get();
        let revision_calls_after_registration = revision_calls.get();
        for _ in 0..5 {
            pv.render(area, &mut buf);
        }

        assert_eq!(height_calls_after_layout, 100);
        assert_eq!(height_calls.get(), height_calls_after_layout);
        assert_eq!(revision_calls_after_registration, 100);
        assert_eq!(revision_calls.get(), revision_calls_after_registration);
        assert!(pv.dynamic_layout_revisions.is_empty());
    }

    #[test]
    fn production_transcript_renderers_slice_huge_caches_to_visible_rows() {
        let line_count = 20_000usize;
        let lines = (0..line_count)
            .map(|index| Line::from(format!("row-{index:05}")))
            .collect::<Vec<_>>();
        let cell = Arc::new(CountingCell {
            calls: Arc::new(AtomicUsize::new(0)),
            lines: lines.clone(),
            is_stream_continuation: false,
        }) as Arc<dyn HistoryCell>;
        let cell_renderable =
            CellRenderable::new(cell, Style::default(), TranscriptDetailMode::Full);
        let hyperlink_renderable =
            HyperlinkLinesRenderable::new(lines.into_iter().map(HyperlinkLine::from).collect());
        let area = Rect::new(0, 0, 20, 3);
        let mut cell_buf = Buffer::empty(area);
        let mut hyperlink_buf = Buffer::empty(area);
        let scroll_offset = line_count - usize::from(area.height);

        cell_renderable.render_with_offset(area, &mut cell_buf, scroll_offset);
        hyperlink_renderable.render_with_offset(area, &mut hyperlink_buf, scroll_offset);

        cell_renderable.with_render_cache(area.width, |cache| {
            assert_eq!(cache.rows.rows.len(), line_count);
            assert_eq!(
                cache.rows.viewport(scroll_offset, area.height).len(),
                usize::from(area.height)
            );
        });
        hyperlink_renderable.with_render_cache(area.width, |cache| {
            assert_eq!(cache.rows.len(), line_count);
            assert_eq!(
                cache.viewport(scroll_offset, area.height).len(),
                usize::from(area.height)
            );
        });
        let expected = format!("row-{:05}", line_count - usize::from(area.height));
        assert!(buffer_to_text(&cell_buf, area).starts_with(&expected));
        assert!(buffer_to_text(&hyperlink_buf, area).starts_with(&expected));
    }

    #[test]
    fn static_overlay_renders_lines_beyond_u16_max_without_a_giant_buffer() {
        let row_count = usize::from(u16::MAX) + 3;
        let mut lines = vec![Line::from("middle"); row_count];
        lines[row_count - 2] = Line::from("deep-tail");
        lines[row_count - 1] = Line::from("last");
        let mut overlay = static_overlay(lines, "T");
        let area = Rect::new(0, 0, 20, 8);
        let content_area = overlay.view.content_area(Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(3),
        ));
        overlay.view.scroll_offset = usize::MAX;
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);

        let rendered = buffer_to_text(&buf, content_area);
        assert!(rendered.contains("deep-tail"));
        assert!(rendered.contains("last"));
    }

    #[test]
    fn pager_view_preserves_transcript_offsets_beyond_u16_max() {
        let oversized_height = usize::from(u16::MAX) + 2;
        let mut lines = vec![HyperlinkLine::from("row"); oversized_height - 1];
        lines.push(HyperlinkLine::from("last"));
        let mut pv = pager_view(
            vec![
                Box::new(CachedRenderable::new(HyperlinkLinesRenderable::new(lines))),
                Box::new(CachedRenderable::new(HyperlinkLinesRenderable::new(vec![
                    HyperlinkLine::from("after"),
                ]))),
            ],
            "T",
            /*scroll_offset*/ oversized_height - 1,
        );
        let area = Rect::new(0, 0, 12, 2);
        let mut buf = Buffer::empty(area);

        pv.refresh_layout(area.width);
        pv.render_content(area, &mut buf);

        assert_eq!(
            pv.chunk_bottoms,
            vec![oversized_height, oversized_height + 1]
        );
        assert_eq!(buf[(0, 0)].symbol(), "l");
        assert_eq!(buf[(0, 1)].symbol(), "a");
    }

    #[test]
    fn inset_renderable_forwards_scroll_past_top_inset() {
        let renderable = InsetRenderable::new(
            paragraph_block("line-", /*lines*/ 5),
            Insets::tlbr(
                /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
            ),
        );
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);

        renderable.render_with_offset(area, &mut buf, /*scroll_offset*/ 2);

        let rendered = buffer_to_text(&buf, area);
        assert!(rendered.starts_with("line-1"));
        assert!(rendered.contains("line-3"));
    }

    #[test]
    fn inset_legacy_height_saturates_for_max_child_and_narrow_width() {
        let renderable = InsetRenderable::new(
            Box::new(MaxHeightRenderable) as Box<dyn Renderable>,
            Insets::tlbr(
                /*top*/ 1, /*left*/ 4, /*bottom*/ 1, /*right*/ 4,
            ),
        );

        assert_eq!(renderable.desired_height(/*width*/ 3), u16::MAX);
        assert_eq!(
            renderable.desired_height_usize(/*width*/ 3),
            usize::from(u16::MAX) + 2
        );
    }

    #[test]
    fn blocked_paragraph_offset_scrolls_the_block_with_its_content() {
        let paragraph = Paragraph::new(vec!["one".into(), "two".into(), "three".into()])
            .block(Block::default().borders(Borders::ALL));
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 3,
        );
        let mut buf = Buffer::empty(area);

        paragraph.render_with_offset(area, &mut buf, /*scroll_offset*/ 2);

        assert_eq!(buf[(0, 0)].symbol(), "│");
        assert_eq!(buf[(1, 0)].symbol(), "t");
        assert_eq!(buf[(0, 1)].symbol(), "│");
        assert_eq!(buf[(1, 1)].symbol(), "t");
        assert_eq!(buf[(0, 2)].symbol(), "└");
        assert_eq!(buf[(area.right() - 1, 2)].symbol(), "┘");
    }

    #[test]
    fn inset_offset_preserves_visible_bottom_inset() {
        let renderable = InsetRenderable::new(
            Box::new(
                Paragraph::new(vec!["first".into(), "second".into()])
                    .style(Style::default().bg(Color::Red)),
            ) as Box<dyn Renderable>,
            Insets::tlbr(
                /*top*/ 0, /*left*/ 0, /*bottom*/ 1, /*right*/ 0,
            ),
        );
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 2,
        );
        let mut buf = Buffer::empty(area);
        buf.set_style(area, Style::default().bg(Color::Blue));

        renderable.render_with_offset(area, &mut buf, /*scroll_offset*/ 1);

        assert_eq!(buf[(0, 0)].symbol(), "s");
        assert_eq!(buf[(0, 0)].bg, Color::Red);
        assert_eq!(buf[(0, 1)].symbol(), " ");
        assert_eq!(buf[(0, 1)].bg, Color::Blue);
    }

    #[test]
    fn pager_view_append_preserves_existing_layout_measurements() {
        let first_height_calls = Rc::new(StdCell::new(0));
        let second_height_calls = Rc::new(StdCell::new(0));
        let mut pv = pager_view(
            vec![Box::new(CountingRenderable {
                height: 5,
                height_calls: Rc::clone(&first_height_calls),
                revision_calls: Rc::new(StdCell::new(0)),
                rendered_offsets: Rc::new(std::cell::RefCell::new(Vec::new())),
            })],
            "T",
            /*scroll_offset*/ 0,
        );
        pv.refresh_layout(/*width*/ 40);

        pv.push_renderable(Box::new(CountingRenderable {
            height: 7,
            height_calls: Rc::clone(&second_height_calls),
            revision_calls: Rc::new(StdCell::new(0)),
            rendered_offsets: Rc::new(std::cell::RefCell::new(Vec::new())),
        }));
        pv.refresh_layout(/*width*/ 40);

        assert_eq!(pv.content_height(), 12);
        assert_eq!(first_height_calls.get(), 1);
        assert_eq!(second_height_calls.get(), 1);
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_down_when_needed() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 1),
                paragraph_block("b", /*lines*/ 3),
                paragraph_block("c", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 8);

        pv.scroll_offset = 0;
        let content_area = pv.content_area(area);
        pv.ensure_chunk_visible(/*idx*/ 2, content_area);
        assert_eq!(pv.scroll_offset, 1);

        let mut buf = Buffer::empty(area);
        pv.render(area, &mut buf);
        let rendered = buffer_to_text(&buf, area);

        assert!(
            rendered.contains("c0"),
            "expected chunk top in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c1"),
            "expected chunk middle in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c2"),
            "expected chunk bottom in view: {rendered:?}"
        );
    }

    #[test]
    fn pager_view_ensure_chunk_visible_does_not_scroll_exact_fit() {
        let mut pv = pager_view(
            vec![paragraph_block("a", /*lines*/ 3)],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 3);

        pv.refresh_layout(area.width);
        pv.ensure_chunk_visible(/*idx*/ 0, area);

        assert_eq!(pv.scroll_offset, 0);
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_up_when_needed() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 2),
                paragraph_block("b", /*lines*/ 3),
                paragraph_block("c", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 3);

        pv.scroll_offset = 6;
        pv.ensure_chunk_visible(/*idx*/ 0, area);

        assert_eq!(pv.scroll_offset, 0);
    }

    #[test]
    fn pager_view_is_scrolled_to_bottom_accounts_for_wrapped_height() {
        let mut pv = pager_view(
            vec![paragraph_block("a", /*lines*/ 10)],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 8);
        let mut buf = Buffer::empty(area);

        pv.render(area, &mut buf);

        assert!(
            !pv.is_scrolled_to_bottom(),
            "expected view to report not at bottom when offset < max"
        );

        pv.scroll_offset = usize::MAX;
        pv.render(area, &mut buf);

        assert!(
            pv.is_scrolled_to_bottom(),
            "expected view to report at bottom after scrolling to end"
        );
    }
}
