use std::io::Result;
use std::sync::Arc;

use crate::history_cell::HistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBindingListExt;
use crate::key_hint::ShortcutHint;
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

use super::TranscriptOverlay;
use super::first_or_empty;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranscriptFlavor {
    /// Follows the displayed thread and permits prompt rollback.
    LiveReviewBrowser,
    /// Browses a fixed transcript with the same detail/navigation controls but no prompt rollback.
    InspectionReviewBrowser,
}

impl TranscriptFlavor {
    pub(super) fn tracks_active_thread(self) -> bool {
        self == Self::LiveReviewBrowser
    }

    pub(super) fn allows_backtrack(self) -> bool {
        self.tracks_active_thread()
    }
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
        Self {
            flavor,
            detail_mode: TranscriptDetailMode::Review,
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
        self.detail_mode = match self.detail_mode {
            TranscriptDetailMode::Review => TranscriptDetailMode::Full,
            TranscriptDetailMode::Full => TranscriptDetailMode::Review,
        };
    }

    pub(super) fn clear_review_target(&mut self) {
        self.selected_review_target = None;
    }

    #[cfg(test)]
    pub(super) fn selected_review_target(self) -> Option<usize> {
        self.selected_review_target
    }

    pub(super) fn select_review_target(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        first_visible_cell: usize,
        direction: TranscriptNavigationDirection,
    ) -> Option<usize> {
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

    pub(super) fn prepend(&mut self, insert_at: usize, added_cells: usize) {
        if let Some(selected) = self.selected_review_target.as_mut()
            && *selected >= insert_at
        {
            *selected = selected.saturating_add(added_cells);
        }
    }
}

fn key_hint_line(pairs: &[(Vec<ShortcutHint>, &str)]) -> Line<'static> {
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
            spans.push(Span::from(*key));
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
    pairs: &[(Vec<ShortcutHint>, &str)],
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
    Paragraph::new(vec![key_hint_line(&fitted)]).render(area, buf);
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
    match browser.detail_mode() {
        TranscriptDetailMode::Review => "T R A N S C R I P T · R E V I E W".to_string(),
        TranscriptDetailMode::Full => "T R A N S C R I P T · F U L L".to_string(),
    }
}

fn transcript_title_for_width(browser: TranscriptBrowserState, width: u16) -> String {
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

    pub(crate) fn tracks_active_thread(&self) -> bool {
        self.browser.flavor().tracks_active_thread()
    }

    pub(crate) fn allows_backtrack(&self) -> bool {
        self.browser.flavor().allows_backtrack()
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
        let anchor = self
            .view
            .pending_align_chunk_top
            .unwrap_or_else(|| self.view.first_visible_chunk())
            .min(self.cells.len().saturating_sub(1));
        self.browser.toggle_detail_mode();
        let _ = self.take_live_tail_renderable();
        self.live_tail_key = None;
        self.rebuild_renderables();
        if !self.cells.is_empty()
            && self.view.pending_scroll_chunk.is_none()
            && self.view.pending_align_chunk_top.is_none()
        {
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
        let pager_pairs = [
            (
                first_or_empty(&self.view.keymap, "scroll_up", &self.view.keymap.scroll_up)
                    .into_iter()
                    .chain(first_or_empty(
                        &self.view.keymap,
                        "scroll_down",
                        &self.view.keymap.scroll_down,
                    ))
                    .collect(),
                "to scroll",
            ),
            (
                first_or_empty(&self.view.keymap, "page_up", &self.view.keymap.page_up)
                    .into_iter()
                    .chain(first_or_empty(
                        &self.view.keymap,
                        "page_down",
                        &self.view.keymap.page_down,
                    ))
                    .collect(),
                "to page",
            ),
            (
                first_or_empty(&self.view.keymap, "jump_top", &self.view.keymap.jump_top)
                    .into_iter()
                    .chain(first_or_empty(
                        &self.view.keymap,
                        "jump_bottom",
                        &self.view.keymap.jump_bottom,
                    ))
                    .collect(),
                "to jump",
            ),
        ];
        Clear.render(line1, buf);
        Clear.render(line2, buf);
        if self.highlight_cell.is_some() && self.allows_backtrack() {
            let _ = render_key_hints_fitting(line1, buf, &pager_pairs);
            let _ = render_key_hints_fitting(
                line2,
                buf,
                &[
                    (
                        first_or_empty(&self.view.keymap, "close", &self.view.keymap.close),
                        "to quit",
                    ),
                    (
                        vec![
                            key_hint::plain(KeyCode::Esc).into(),
                            key_hint::plain(KeyCode::Left).into(),
                        ],
                        "to edit prev",
                    ),
                    (vec![key_hint::plain(KeyCode::Right).into()], "to edit next"),
                    (
                        vec![key_hint::plain(KeyCode::Enter).into()],
                        "to edit message",
                    ),
                ],
            );
            return;
        }
        let _ = render_key_hints_fitting(line1, buf, &pager_pairs);
        let _ = render_key_hints_fitting(
            line2,
            buf,
            &[
                (
                    first_or_empty(&self.view.keymap, "close", &self.view.keymap.close),
                    "close",
                ),
                (vec![key_hint::plain(KeyCode::Char('v')).into()], "detail"),
                (
                    vec![key_hint::plain(KeyCode::Char('[')).into()],
                    "review prev",
                ),
                (
                    vec![key_hint::plain(KeyCode::Char(']')).into()],
                    "review next",
                ),
            ],
        );
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 3);
        self.view.title = transcript_title_for_width(self.browser, area.width);
        self.view.render(top, buf);
        self.render_history_state(top, buf);
        self.render_hints(bottom, buf);
        self.highlight_draw_pending = self.highlight_cell.is_some_and(|highlight_cell| {
            let Some(&highlight_bottom) = self.view.chunk_bottoms.get(highlight_cell) else {
                return true;
            };
            let highlight_top = highlight_cell
                .checked_sub(1)
                .and_then(|index| self.view.chunk_bottoms.get(index))
                .copied()
                .unwrap_or(0);
            let highlight_content_top = highlight_top.saturating_add(usize::from(
                highlight_cell > 0
                    && self
                        .cells
                        .get(highlight_cell)
                        .is_some_and(|cell| !cell.is_stream_continuation()),
            ));
            let viewport_bottom = self
                .view
                .scroll_offset
                .saturating_add(self.view.last_content_height.unwrap_or(0));
            highlight_content_top >= viewport_bottom || highlight_bottom <= self.view.scroll_offset
        });
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if self.is_close_key(e) => {
                    self.is_done = true;
                    Ok(())
                }
                KeyEvent {
                    code: KeyCode::Esc,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press | KeyEventKind::Repeat,
                    ..
                } if self.browser.flavor() == TranscriptFlavor::InspectionReviewBrowser => {
                    self.is_done = true;
                    Ok(())
                }
                e if self.view.last_content_height.is_none()
                    && (is_plain_char(e, 'v')
                        || is_review_navigation_char(e, '[')
                        || is_review_navigation_char(e, ']')) =>
                {
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if is_plain_char(e, 'v') => {
                    self.toggle_detail_mode();
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if is_review_navigation_char(e, '[') => {
                    self.navigate_review_target(TranscriptNavigationDirection::Previous);
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                e if is_review_navigation_char(e, ']') => {
                    self.navigate_review_target(TranscriptNavigationDirection::Next);
                    tui.frame_requester().schedule_frame();
                    Ok(())
                }
                other => {
                    if self.view.is_scroll_key(other) {
                        self.browser.clear_review_target();
                        if self.highlight_cell.is_some() {
                            // Enter must not confirm a selection after scrolling until a frame
                            // proves that the highlighted target is still visible.
                            self.highlight_draw_pending = true;
                        }
                    }
                    self.view.handle_key_event(tui, other)
                }
            },
            TuiEvent::Draw | TuiEvent::Resume | TuiEvent::Resize(_) => {
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
