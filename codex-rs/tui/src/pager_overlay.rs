//! Static pager overlays and standalone transcript adapters.
//!
//! Static content retains its generic pager. Transcript previews share the main conversation
//! viewport, including its scrolling, selection, search and bounded text layouts.

mod cached_rows;
mod scrolling;
mod transcript;
mod view;

use cached_rows::CachedRows;
use view::PagerView;

pub(crate) use transcript::TranscriptOverlay;
use transcript::TranscriptOverlayScope;

#[cfg(test)]
#[path = "pager_overlay/transcript_tests.rs"]
mod transcript_tests;

use std::io::Result;
use std::sync::Arc;

use crate::chatwidget::ActiveCellTranscriptKey;
use crate::history_cell::HistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::key_hint::KeyBindingListExt;
use crate::key_hint::ShortcutHint;
use crate::keymap::PagerKeymap;
use crate::render::renderable::Renderable;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

pub(crate) enum Overlay {
    Transcript(TranscriptOverlay),
    Static(StaticOverlay),
    Analytics(Box<crate::analytics::AnalyticsView>),
}
impl Overlay {
    pub(crate) fn new_review_transcript(
        cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
    ) -> Self {
        let mut overlay = TranscriptOverlay::new(cells, keymap);
        overlay
            .view
            .open_review_browser(crate::history_cell::HistoryRenderMode::Rich);
        Self::Transcript(overlay)
    }

    pub(crate) fn new_inspection_transcript(
        cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
    ) -> Self {
        let mut overlay =
            TranscriptOverlay::new_scoped(cells, keymap, TranscriptOverlayScope::FixedInspection);
        overlay
            .view
            .open_review_browser(crate::history_cell::HistoryRenderMode::Rich);
        Self::Transcript(overlay)
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
        let input = match self {
            Overlay::Transcript(_) => tui::OverlayInput::Transcript,
            Overlay::Static(_) => tui::OverlayInput::StaticPager,
            Overlay::Analytics(_) => tui::OverlayInput::Usage,
        };
        tui.set_overlay_input(input)?;
        let result = match self {
            Overlay::Transcript(o) => o.handle_event(tui, event),
            Overlay::Static(o) => o.handle_event(tui, event),
            Overlay::Analytics(o) => o.handle_event(tui, event),
        };
        if result.is_err() || self.is_done() {
            let restore = tui.set_overlay_input(tui::OverlayInput::Default);
            return result.and(restore);
        }
        result
    }

    pub(crate) fn is_done(&self) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_done(),
            Overlay::Static(o) => o.is_done(),
            Overlay::Analytics(o) => o.is_done,
        }
    }

    /// Returns the transcript that mirrors the displayed thread, excluding fixed inspections.
    pub(crate) fn active_transcript_mut(&mut self) -> Option<&mut TranscriptOverlay> {
        match self {
            Self::Transcript(transcript) if transcript.tracks_active_thread() => Some(transcript),
            Self::Transcript(_) | Self::Static(_) | Self::Analytics(_) => None,
        }
    }
}

fn first_or_empty(
    keymap: &PagerKeymap,
    action: &'static str,
    bindings: &[KeyBinding],
) -> Vec<ShortcutHint> {
    keymap.primary_hint(action, bindings).into_iter().collect()
}

// Render a single line of key hints from (key(s), description) pairs.
fn render_key_hints(area: Rect, buf: &mut Buffer, pairs: &[(Vec<ShortcutHint>, &str)]) {
    let mut spans: Vec<Span<'static>> = vec![" ".into()];
    let mut first = true;
    for (keys, desc) in pairs {
        if !first {
            spans.push(" · ".dim());
        }
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                spans.extend(crate::key_hint::key_label_spans("/"));
            }
            spans.extend(key.spans());
        }
        spans.push(" ".into());
        spans.push(Span::from(desc.to_string()));
        first = false;
    }
    Paragraph::new(vec![Line::from(spans).dim()]).render(area, buf);
}

fn render_navigation_hints(area: Rect, buf: &mut Buffer, keymap: &PagerKeymap) {
    let actions = [
        ("scroll_up", &keymap.scroll_up),
        ("scroll_down", &keymap.scroll_down),
        ("page_up", &keymap.page_up),
        ("page_down", &keymap.page_down),
        ("jump_top", &keymap.jump_top),
        ("jump_bottom", &keymap.jump_bottom),
    ];
    let hints = actions
        .chunks_exact(2)
        .zip(["to scroll", "to page", "to jump"])
        .map(|(actions, description)| {
            (
                actions
                    .iter()
                    .filter_map(|(action, bindings)| keymap.primary_hint(action, bindings))
                    .collect(),
                description,
            )
        })
        .collect::<Vec<_>>();
    render_key_hints(area, buf, &hints);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TranscriptHistoryState {
    #[default]
    Idle,
    LoadingOlder,
    LoadingBeginning,
    Partial,
    Failed,
    Complete,
}

impl TranscriptHistoryState {
    pub(crate) fn has_unloaded_history(self) -> bool {
        matches!(
            self,
            Self::LoadingOlder | Self::LoadingBeginning | Self::Partial | Self::Failed
        )
    }
}

pub(crate) struct StaticOverlay {
    view: PagerView,
    is_done: bool,
}

impl StaticOverlay {
    const HINTS_HEIGHT: u16 = 3;

    pub(crate) fn with_title(
        lines: Vec<Line<'static>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::with_renderables(vec![Box::new(CachedRows::new(lines))], title, keymap)
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
        if area.is_empty() {
            return;
        }
        let line1 = Rect::new(area.x, area.y, area.width, 1);
        let line2 = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1).min(1),
        );
        render_navigation_hints(line1, buf, &self.view.keymap);
        let pairs: Vec<(Vec<ShortcutHint>, &str)> = vec![(
            first_or_empty(&self.view.keymap, "close", &self.view.keymap.close),
            "close",
        )];
        render_key_hints(line2, buf, &pairs);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(Self::HINTS_HEIGHT);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(
            area.x,
            area.y + top_h,
            area.width,
            area.height.min(Self::HINTS_HEIGHT),
        );
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
            TuiEvent::Draw | TuiEvent::Resume | TuiEvent::Resize(_) | TuiEvent::FocusGained => {
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
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn default_pager_keymap() -> crate::keymap::PagerKeymap {
        crate::keymap::RuntimeKeymap::defaults().pager
    }

    #[test]
    fn inspection_transcript_is_fixed_while_review_transcript_tracks_active_thread() {
        let mut active = Overlay::new_review_transcript(Vec::new(), default_pager_keymap());
        let mut inspection = Overlay::new_inspection_transcript(Vec::new(), default_pager_keymap());

        assert!(active.active_transcript_mut().is_some());
        assert!(inspection.active_transcript_mut().is_none());
    }

    fn static_overlay(lines: Vec<Line<'static>>, title: &str) -> StaticOverlay {
        StaticOverlay::with_title(lines, title.to_string(), default_pager_keymap())
    }

    #[test]
    fn footer_hints_display_chords_without_internal_dispatch_keys() {
        use codex_config::types::KeybindingSpec;
        use codex_config::types::KeybindingsSpec;
        use codex_config::types::TuiKeymap;

        let mut config = TuiKeymap::default();
        config.pager.page_up = Some(KeybindingsSpec::One(KeybindingSpec(
            "ctrl-x page-up".to_string(),
        )));
        let keymap = crate::keymap::RuntimeKeymap::from_config(&config).expect("valid pager chord");

        assert_eq!(
            first_or_empty(&keymap.pager, "page_up", &keymap.pager.page_up),
            vec![ShortcutHint::Chord {
                prefix: key_hint::ctrl(KeyCode::Char('x')),
                completion: key_hint::plain(KeyCode::PageUp),
            }]
        );
    }

    #[test]

    fn static_overlay_snapshot_basic() {
        // Prepare a static overlay with a few lines and a title
        let mut overlay = static_overlay(
            vec!["one".into(), "two".into(), "three".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(/*width*/ 40, /*height*/ 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn static_overlay_wraps_long_lines() {
        let mut overlay = static_overlay(
            vec!["a very long line that should wrap when rendered within a narrow pager overlay width".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(/*width*/ 24, /*height*/ 8)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }
}
