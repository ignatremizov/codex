use std::io::Result;
use std::sync::Arc;

use crate::history_cell::HistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::key_hint::KeyBindingListExt;
use crate::key_hint::is_altgr;
use crate::tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;

use super::TranscriptOverlay;
use super::first_or_empty;
use super::render_key_hints;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptFlavor {
    LiveReviewBrowser,
    HistoricalFullPreview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptDetailMode {
    Review,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptNavigationDirection {
    Previous,
    Next,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TranscriptBrowserState {
    flavor: TranscriptFlavor,
    detail_mode: TranscriptDetailMode,
    selected_review_target: Option<usize>,
}

impl TranscriptBrowserState {
    pub(super) fn new(flavor: TranscriptFlavor) -> Self {
        let detail_mode = match flavor {
            TranscriptFlavor::LiveReviewBrowser => TranscriptDetailMode::Review,
            TranscriptFlavor::HistoricalFullPreview => TranscriptDetailMode::Full,
        };
        Self {
            flavor,
            detail_mode,
            selected_review_target: None,
        }
    }

    pub(super) fn flavor(self) -> TranscriptFlavor {
        self.flavor
    }

    pub(super) fn detail_mode(self) -> TranscriptDetailMode {
        self.detail_mode
    }

    pub(super) fn toggle_detail_mode(&mut self) {
        if self.flavor != TranscriptFlavor::LiveReviewBrowser {
            return;
        }
        self.detail_mode = match self.detail_mode {
            TranscriptDetailMode::Review => TranscriptDetailMode::Full,
            TranscriptDetailMode::Full => TranscriptDetailMode::Review,
        };
    }

    pub(super) fn clear_review_target(&mut self) {
        self.selected_review_target = None;
    }

    pub(super) fn selected_review_target(self) -> Option<usize> {
        self.selected_review_target
    }

    pub(super) fn select_review_target(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        first_visible_cell: usize,
        direction: TranscriptNavigationDirection,
    ) -> Option<usize> {
        if self.flavor != TranscriptFlavor::LiveReviewBrowser {
            return None;
        }
        let selected = match (self.selected_review_target, direction) {
            (Some(selected), TranscriptNavigationDirection::Previous) => cells
                .iter()
                .enumerate()
                .take(selected)
                .rev()
                .find(|(_, cell)| cell.transcript_navigation_kind().is_some())
                .map(|(index, _)| index),
            (Some(selected), TranscriptNavigationDirection::Next) => cells
                .iter()
                .enumerate()
                .skip(selected.saturating_add(1))
                .find(|(_, cell)| cell.transcript_navigation_kind().is_some())
                .map(|(index, _)| index),
            (None, TranscriptNavigationDirection::Previous) => cells
                .iter()
                .enumerate()
                .take(first_visible_cell.saturating_add(1))
                .rev()
                .find(|(_, cell)| cell.transcript_navigation_kind().is_some())
                .map(|(index, _)| index),
            (None, TranscriptNavigationDirection::Next) => cells
                .iter()
                .enumerate()
                .skip(first_visible_cell)
                .find(|(_, cell)| cell.transcript_navigation_kind().is_some())
                .map(|(index, _)| index),
        };
        if selected.is_some() {
            self.selected_review_target = selected;
        }
        selected
    }

    pub(super) fn consolidate(&mut self, start: usize, end: usize, consolidated_is_target: bool) {
        let Some(selected) = self.selected_review_target else {
            return;
        };
        if selected < start {
            return;
        }
        if selected < end {
            self.selected_review_target = consolidated_is_target.then_some(start);
            return;
        }
        let removed = end.saturating_sub(start);
        self.selected_review_target = Some(selected.saturating_sub(removed.saturating_sub(1)));
    }
}

fn key_hint_line(pairs: &[(Vec<KeyBinding>, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![" ".into()];
    let mut first = true;
    for (keys, desc) in pairs {
        if !first {
            spans.push("   ".into());
        }
        for (index, key) in keys.iter().enumerate() {
            if index > 0 {
                spans.push("/".into());
            }
            spans.push(Span::from(key));
        }
        spans.push(" ".into());
        spans.push(Span::from(desc.to_string()));
        first = false;
    }
    Line::from(spans).dim()
}

fn render_key_hints_fitting(
    area: Rect,
    buf: &mut Buffer,
    pairs: &[(Vec<KeyBinding>, &str)],
) -> bool {
    let mut fitted = Vec::new();
    for pair in pairs {
        let mut candidate = fitted.clone();
        candidate.push(pair.clone());
        if key_hint_line(&candidate).width() > usize::from(area.width) {
            break;
        }
        fitted = candidate;
    }
    Paragraph::new(vec![key_hint_line(&fitted)]).render_ref(area, buf);
    fitted.len() == pairs.len()
}

fn is_plain_char(key_event: KeyEvent, character: char) -> bool {
    matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key_event.modifiers == KeyModifiers::NONE
        && key_event.code == KeyCode::Char(character)
}

fn is_review_navigation_char(key_event: KeyEvent, character: char) -> bool {
    matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && (key_event.modifiers == KeyModifiers::NONE || is_altgr(key_event.modifiers))
        && key_event.code == KeyCode::Char(character)
}

pub(super) fn transcript_title(browser: TranscriptBrowserState) -> String {
    match (browser.flavor(), browser.detail_mode()) {
        (TranscriptFlavor::HistoricalFullPreview, _) => "T R A N S C R I P T".to_string(),
        (TranscriptFlavor::LiveReviewBrowser, TranscriptDetailMode::Review) => {
            "T R A N S C R I P T · R E V I E W".to_string()
        }
        (TranscriptFlavor::LiveReviewBrowser, TranscriptDetailMode::Full) => {
            "T R A N S C R I P T · F U L L".to_string()
        }
    }
}

fn transcript_title_for_width(browser: TranscriptBrowserState, width: u16) -> String {
    if browser.flavor() == TranscriptFlavor::HistoricalFullPreview {
        return transcript_title(browser);
    }
    let candidates = match browser.detail_mode() {
        TranscriptDetailMode::Review => [
            "T R A N S C R I P T · R E V I E W",
            "TRANSCRIPT · REVIEW",
            "REVIEW",
        ],
        TranscriptDetailMode::Full => {
            ["T R A N S C R I P T · F U L L", "TRANSCRIPT · FULL", "FULL"]
        }
    };
    let available = usize::from(width.saturating_sub(2));
    candidates
        .into_iter()
        .find(|candidate| candidate.chars().count() <= available)
        .map(str::to_string)
        .unwrap_or_else(|| candidates[2].chars().take(available).collect())
}

impl TranscriptOverlay {
    pub(crate) fn is_review_mode(&self) -> bool {
        self.browser.detail_mode() == TranscriptDetailMode::Review
    }

    #[cfg(test)]
    pub(crate) fn selected_review_target(&self) -> Option<usize> {
        self.browser.selected_review_target()
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> usize {
        self.view.scroll_offset
    }

    pub(crate) fn is_close_key(&self, key_event: KeyEvent) -> bool {
        self.view.keymap.close.is_pressed(key_event)
            || self.view.keymap.close_transcript.is_pressed(key_event)
    }

    fn toggle_detail_mode(&mut self) {
        if self.browser.flavor() != TranscriptFlavor::LiveReviewBrowser {
            return;
        }
        let anchor = self
            .view
            .pending_align_chunk_top
            .unwrap_or_else(|| self.view.first_visible_chunk())
            .min(self.cells.len().saturating_sub(1));
        self.browser.toggle_detail_mode();
        self.take_live_tail_renderable();
        self.live_tail_key = None;
        self.rebuild_renderables();
        if !self.cells.is_empty() {
            self.view.align_chunk_to_top(anchor);
        }
    }

    fn navigate_review_target(&mut self, direction: TranscriptNavigationDirection) {
        let first_visible = match direction {
            TranscriptNavigationDirection::Previous => self
                .view
                .first_visible_chunk()
                .min(self.cells.len().saturating_sub(1)),
            TranscriptNavigationDirection::Next => self
                .view
                .first_chunk_starting_at_or_below_top()
                .min(self.cells.len()),
        };
        if let Some(target) =
            self.browser
                .select_review_target(&self.cells, first_visible, direction)
        {
            self.view.align_chunk_to_top(target);
        }
    }

    fn render_hints(&self, area: Rect, buf: &mut Buffer) {
        let line1 = Rect::new(area.x, area.y, area.width, 1);
        let line2 = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
        if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser {
            Clear.render(line1, buf);
            Clear.render(line2, buf);
            if self.highlight_cell.is_some() {
                let _ = render_key_hints_fitting(
                    line2,
                    buf,
                    &[
                        (first_or_empty(&self.view.keymap.close), "to quit"),
                        (
                            vec![
                                key_hint::plain(KeyCode::Esc),
                                key_hint::plain(KeyCode::Left),
                            ],
                            "to edit prev",
                        ),
                        (vec![key_hint::plain(KeyCode::Right)], "to edit next"),
                        (vec![key_hint::plain(KeyCode::Enter)], "to edit message"),
                    ],
                );
                return;
            }
            let all_browser_hints_fit = render_key_hints_fitting(
                line2,
                buf,
                &[
                    (first_or_empty(&self.view.keymap.close), "close"),
                    (vec![key_hint::plain(KeyCode::Char('v'))], "detail"),
                    (
                        vec![
                            key_hint::plain(KeyCode::Char('[')),
                            key_hint::plain(KeyCode::Char(']')),
                        ],
                        "review items",
                    ),
                ],
            );
            if !all_browser_hints_fit {
                return;
            }
        }
        let pager_pairs = [
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
        ];
        if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser {
            let _ = render_key_hints_fitting(line1, buf, &pager_pairs);
            return;
        }
        render_key_hints(line1, buf, &pager_pairs);
        let mut pairs: Vec<(Vec<KeyBinding>, &str)> =
            vec![(first_or_empty(&self.view.keymap.close), "to quit")];
        if self.highlight_cell.is_some() {
            pairs.push((
                vec![
                    key_hint::plain(KeyCode::Esc),
                    key_hint::plain(KeyCode::Left),
                ],
                "to edit prev",
            ));
            pairs.push((vec![key_hint::plain(KeyCode::Right)], "to edit next"));
            pairs.push((vec![key_hint::plain(KeyCode::Enter)], "to edit message"));
        } else {
            pairs.push((vec![key_hint::plain(KeyCode::Esc)], "to edit prev"));
        }
        render_key_hints(line2, buf, &pairs);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 3);
        self.view.title = transcript_title_for_width(self.browser, area.width);
        self.view.render(top, buf);
        self.render_hints(bottom, buf);
        self.highlight_draw_pending = false;
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if self.is_close_key(e) => {
                    self.is_done = true;
                    Ok(())
                }
                e if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser
                    && self.view.last_content_height.is_none()
                    && (is_plain_char(e, 'v')
                        || is_review_navigation_char(e, '[')
                        || is_review_navigation_char(e, ']')) =>
                {
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser
                    && is_plain_char(e, 'v') =>
                {
                    self.toggle_detail_mode();
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser
                    && is_review_navigation_char(e, '[') =>
                {
                    self.navigate_review_target(TranscriptNavigationDirection::Previous);
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if self.browser.flavor() == TranscriptFlavor::LiveReviewBrowser
                    && is_review_navigation_char(e, ']') =>
                {
                    self.navigate_review_target(TranscriptNavigationDirection::Next);
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                other => {
                    if self.view.is_scroll_key(other) {
                        self.browser.clear_review_target();
                    }
                    self.view.handle_key_event(tui, other)
                }
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
#[path = "transcript_tests.rs"]
mod tests;
