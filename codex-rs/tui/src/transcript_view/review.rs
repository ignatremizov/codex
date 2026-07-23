//! Live transcript review reuses the viewport's cell identities, anchors and layout cache.
//! Historical previews and the ordinary conversation never claim browser character keys.

use super::*;
use crate::bottom_pane::TranscriptFooter;
use crate::key_hint::is_altgr;
use crate::key_hint::key_label_spans;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewMode {
    Review,
    Full,
}

#[derive(Clone, Copy, Default)]
pub(super) struct ReviewBrowser {
    selected: Option<EntryKey>,
}

impl TranscriptView {
    pub(crate) fn open_review_browser(&mut self, mode: HistoryRenderMode) {
        self.review = Some(ReviewBrowser::default());
        self.clear_activity_focus();
        self.set_review_presentation(/*detailed*/ false, mode);
        // Compact owned activity has disclosure rows; Review uses the cell's display directly.
        self.cache.clear();
        self.visible.clear();
        self.live_key = None;
        self.painted_highlight = None;
    }

    pub(crate) fn close_review_browser(&mut self, mode: HistoryRenderMode) {
        self.set_review_presentation(/*detailed*/ false, mode);
        self.review = None;
        self.cache.clear();
        self.live_key = None;
        self.painted_highlight = None;
    }

    /// Presentation changes invalidate retained layouts, but not removed cells' source identity.
    fn set_review_presentation(&mut self, detailed: bool, mode: HistoryRenderMode) {
        self.selection = None;
        let retained = self.held_reading.take().filter(|_| {
            !matches!(
                self.position,
                Position::Reading(Anchor {
                    key: EntryKey::Live,
                    ..
                })
            )
        });
        self.set_presentation(detailed, mode);
        self.cache.clear();
        self.visible.clear();
        self.live_key = None;
        self.live_separated = None;
        self.painted_highlight = None;
        self.suppressed_prompt_header = None;
        self.invalidate_held_search();
        self.held_reading = retained.map(|mut snapshot| {
            snapshot.refresh_live_presentation = snapshot.pinned.contains_key(&EntryKey::Live);
            snapshot.pinned.clear();
            snapshot.activities.clear();
            snapshot
        });
    }

    pub(crate) fn is_review_browser(&self) -> bool {
        self.review.is_some()
    }

    pub(crate) fn review_mode(&self) -> Option<ReviewMode> {
        self.review.map(|_| {
            if self.detailed {
                ReviewMode::Full
            } else {
                ReviewMode::Review
            }
        })
    }

    pub(crate) fn owns_review_key(&self, key: KeyEvent) -> bool {
        self.review.is_some()
            && key.kind != KeyEventKind::Release
            && match key.code {
                KeyCode::Char('v') => key.modifiers.is_empty(),
                KeyCode::Char('[' | ']') => {
                    key.modifiers.is_empty()
                        || (is_altgr(key.modifiers)
                            && key
                                .modifiers
                                .difference(
                                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
                                )
                                .is_empty())
                }
                _ => false,
            }
    }

    pub(crate) fn handle_review_key(
        &mut self,
        key: KeyEvent,
        cells: &[Arc<dyn HistoryCell>],
    ) -> Option<ViewAction> {
        if !self.owns_review_key(key) || self.has_active_interaction() {
            return None;
        }
        // An unpainted viewport cannot supply a meaningful top-relative navigation anchor.
        if !self.has_painted_layout() {
            return Some(ViewAction::Changed);
        }
        if key.code == KeyCode::Char('v') {
            let (index, row) = self.start(cells);
            let anchor = self.layout(cells, index).map(|layout| Anchor {
                key: self.entry_key(cells, index),
                index,
                offset: layout.position_at(row, /*column*/ 0),
                row_bias: 0,
            });
            let selected = self.review.and_then(|browser| browser.selected);
            self.set_review_presentation(!self.detailed, self.mode);
            let snapshot = self.snapshot_cells();
            let displayed = snapshot.as_deref().unwrap_or(cells);
            if let Some(index) = selected.and_then(|key| {
                displayed
                    .iter()
                    .position(|cell| EntryKey::cell(cell) == key)
            }) {
                self.position = Position::Reading(Anchor {
                    key: EntryKey::cell(&displayed[index]),
                    index,
                    offset: 0,
                    row_bias: 0,
                });
            } else if let Some(anchor) = anchor {
                self.position = Position::Reading(anchor);
            }
            return Some(ViewAction::Changed);
        }
        let snapshot = self.snapshot_cells();
        let displayed = snapshot.as_deref().unwrap_or(cells);
        let selected = self
            .review
            .and_then(|browser| browser.selected)
            .and_then(|key| {
                displayed
                    .iter()
                    .position(|cell| EntryKey::cell(cell) == key)
            });
        let (top, row) = self.start(cells);
        let inside_content = self
            .layout(cells, top)
            .is_some_and(|layout| layout.position_at(row, /*column*/ 0) > 0);
        let next = key.code == KeyCode::Char(']');
        let boundary = selected.map_or_else(
            || {
                if next && inside_content {
                    top.saturating_add(1)
                } else {
                    top
                }
            },
            |index| if next { index.saturating_add(1) } else { index },
        );
        let candidate = if next {
            (boundary..displayed.len())
                .find(|index| displayed[*index].transcript_navigation_kind().is_some())
        } else {
            let end = if selected.is_some() {
                boundary
            } else {
                boundary.saturating_add(1)
            };
            (0..end.min(displayed.len()))
                .rev()
                .find(|index| displayed[*index].transcript_navigation_kind().is_some())
        };
        if let Some(index) = candidate {
            let key = EntryKey::cell(&displayed[index]);
            if let Some(current_index) = cells.iter().position(|cell| EntryKey::cell(cell) == key) {
                self.jump_to_entry(cells, current_index);
            } else {
                self.cancel_beginning();
                self.painted_highlight = None;
                self.position = Position::Reading(Anchor {
                    key,
                    index,
                    offset: 0,
                    row_bias: 0,
                });
            }
            if let Some(browser) = &mut self.review {
                browser.selected = Some(key);
            }
        }
        Some(ViewAction::Changed)
    }

    pub(crate) fn clear_review_target(&mut self) {
        if let Some(browser) = &mut self.review {
            browser.selected = None;
        }
        self.painted_highlight = None;
    }

    /// Confirmation requires painted message content, not a separator or a queued alignment.
    pub(crate) fn highlighted_content_is_drawn(
        &self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> bool {
        self.highlight == Some(index)
            && !self.has_active_interaction()
            && cells.get(index).is_some_and(|cell| {
                self.painted_highlight
                    == Some((EntryKey::cell(cell), cells.last().map(EntryKey::cell)))
            })
    }

    pub(crate) fn has_painted_layout(&self) -> bool {
        !self.area.is_empty() && !self.visible.is_empty()
    }

    pub(crate) fn invalidate_highlight_paint(&mut self) {
        self.painted_highlight = None;
    }

    pub(super) fn remap_review_target(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        range: std::ops::Range<usize>,
        replacement: &Arc<dyn HistoryCell>,
    ) {
        self.painted_highlight = None;
        if let Some(browser) = &mut self.review
            && browser.selected.is_some_and(|selected| {
                cells[range]
                    .iter()
                    .any(|cell| EntryKey::cell(cell) == selected)
            })
        {
            browser.selected = replacement
                .transcript_navigation_kind()
                .map(|_| EntryKey::cell(replacement));
        }
    }

    pub(crate) fn review_title(&self, width: u16) -> Option<Line<'static>> {
        let label = match self.review_mode()? {
            ReviewMode::Review => "REVIEW",
            ReviewMode::Full => "FULL",
        };
        let spaced = label
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        Some(
            [
                format!("T R A N S C R I P T · {spaced}"),
                format!("TRANSCRIPT · {label}"),
                label.to_owned(),
            ]
            .into_iter()
            .map(|title| Line::from(title).dim())
            .find(|title| title.width() <= usize::from(width))
            .unwrap_or_default(),
        )
    }

    pub(crate) fn review_footer(&self, width: u16, close: &str) -> Option<TranscriptFooter> {
        let label = match self.review_mode()? {
            ReviewMode::Review => "Review",
            ReviewMode::Full => "Full",
        };
        let mut line = Line::from(label.fg(crate::style::accent_color()));
        if line.width() > usize::from(width) {
            line = Line::default();
        }
        for (key, action) in [
            (close, "close"),
            ("v", "detail"),
            ("[", "review prev"),
            ("]", "review next"),
        ] {
            let mut candidate = line.clone();
            candidate.spans.push(" · ".dim());
            candidate.spans.extend(key_label_spans(key));
            candidate.spans.push(format!(" {action}").dim());
            if candidate.width() > usize::from(width) {
                break;
            }
            line = candidate;
        }
        Some(TranscriptFooter {
            text: line.into(),
            cursor_column: None,
            is_interactive: true,
        })
    }
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
