use super::*;
use std::collections::HashMap;
use std::path::Path;

use codex_protocol::models::MessagePhase;
use insta::assert_snapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::diff_model::FileChange;
use crate::chatwidget::ActiveCellTranscriptKey;
use crate::history_cell;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::PlainHistoryCell;
use crate::pager_overlay::CellRenderable;
use crate::terminal_hyperlinks::visible_lines;

#[derive(Debug)]
struct ReviewFullCell;

impl HistoryCell for ReviewFullCell {
    fn display_lines(&self, _width: u16) -> Vec<ratatui::text::Line<'static>> {
        vec!["concise file-read preview".into()]
    }

    fn raw_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        vec!["raw".into()]
    }

    fn transcript_lines(&self, _width: u16) -> Vec<ratatui::text::Line<'static>> {
        vec![
            "full file line 1".into(),
            "full file line 2".into(),
            "full file line 3".into(),
        ]
    }
}

fn review_cells() -> Vec<Arc<dyn HistoryCell>> {
    vec![
        Arc::new(PlainHistoryCell::new(vec!["user".into()])),
        Arc::new(AgentMarkdownCell::new_with_phase(
            "commentary".to_string(),
            Path::new("/tmp"),
            Some(MessagePhase::Commentary),
        )),
        Arc::new(AgentMarkdownCell::new_with_phase(
            "final".to_string(),
            Path::new("/tmp"),
            Some(MessagePhase::FinalAnswer),
        )),
        Arc::new(history_cell::new_patch_event(
            HashMap::from([(
                "src/main.rs".into(),
                FileChange::Add {
                    content: "fn main() {}".to_string(),
                },
            )]),
            Path::new("/tmp"),
        )),
    ]
}

#[test]
fn historical_preview_stays_full() {
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::HistoricalFullPreview);

    state.toggle_detail_mode();

    assert_eq!(TranscriptDetailMode::Full, state.detail_mode());
}

#[test]
fn live_browser_toggles_detail_mode() {
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);

    state.toggle_detail_mode();
    assert_eq!(TranscriptDetailMode::Full, state.detail_mode());
    state.toggle_detail_mode();
    assert_eq!(TranscriptDetailMode::Review, state.detail_mode());
}

#[test]
fn review_navigation_visits_commentary_and_patch_cells_only() {
    let cells = review_cells();
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);

    assert_eq!(
        Some(1),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(3),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(1),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Previous)
    );
}

#[test]
fn manual_clear_reanchors_navigation_at_the_viewport() {
    let cells = review_cells();
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);
    let _ = state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next);

    state.clear_review_target();

    assert_eq!(
        Some(3),
        state.select_review_target(&cells, 2, TranscriptNavigationDirection::Next)
    );
}

#[test]
fn consolidation_rebases_selected_target_after_replaced_range() {
    let cells = review_cells();
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);
    let _ = state.select_review_target(&cells, 3, TranscriptNavigationDirection::Previous);

    state.consolidate(
        /*start*/ 0, /*end*/ 2, /*consolidated_is_target*/ false,
    );

    assert_eq!(Some(2), state.selected_review_target());
}

#[test]
fn review_and_full_modes_snapshot_their_distinct_cell_representations() {
    let cell = Arc::new(ReviewFullCell) as Arc<dyn HistoryCell>;
    let render = |mode| {
        let renderable = CellRenderable::new(cell.clone(), Style::default(), mode);
        renderable.with_render_cache(/*width*/ 80, |cache| {
            visible_lines(cache.rows.rows.clone())
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    };

    assert_snapshot!(
        format!(
            "review:\n{}\n\nfull:\n{}",
            render(TranscriptDetailMode::Review),
            render(TranscriptDetailMode::Full)
        ),
        @r"
    review:
    concise file-read preview

    full:
    full file line 1
    full file line 2
    full file line 3"
    );
}

#[test]
fn live_transcript_title_fallbacks_snapshot() {
    let review = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);
    let mut full = review;
    full.toggle_detail_mode();

    assert_snapshot!(
        [
            transcript_title_for_width(review, 80),
            transcript_title_for_width(review, 22),
            transcript_title_for_width(review, 8),
            transcript_title_for_width(review, 5),
            transcript_title_for_width(full, 80),
            transcript_title_for_width(full, 20),
            transcript_title_for_width(full, 6),
            transcript_title_for_width(full, 4),
        ]
        .join("\n"),
        @r"
    T R A N S C R I P T · R E V I E W
    TRANSCRIPT · REVIEW
    REVIEW
    REV
    T R A N S C R I P T · F U L L
    TRANSCRIPT · FULL
    FULL
    FU"
    );
}

#[test]
fn live_overlay_handles_mode_navigation_and_manual_scroll() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        )
        .expect("toggle detail");
    assert_eq!(TranscriptDetailMode::Full, overlay.browser.detail_mode());

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
        )
        .expect("navigate");
    assert_eq!(Some(1), overlay.selected_review_target());

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        )
        .expect("scroll");
    assert_eq!(None, overlay.selected_review_target());
    assert_eq!(None, overlay.view.pending_align_chunk_top);
}

#[test]
fn detail_toggle_invalidates_and_rebuilds_live_tail_in_new_mode() {
    let mut overlay = TranscriptOverlay::new(
        Vec::new(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let key = ActiveCellTranscriptKey {
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    };
    overlay.sync_live_tail(/*width*/ 80, Some(key), |_| {
        Some(vec!["review tail".into()])
    });
    assert_eq!(overlay.view.renderables.len(), 1);
    assert_eq!(
        overlay.live_tail_key.map(|key| key.detail_mode),
        Some(TranscriptDetailMode::Review)
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        )
        .expect("toggle detail");
    assert!(overlay.view.renderables.is_empty());
    overlay.sync_live_tail(/*width*/ 80, Some(key), |_| {
        Some(vec!["full tail".into()])
    });

    assert_eq!(overlay.view.renderables.len(), 1);
    assert_eq!(
        overlay.live_tail_key.map(|key| key.detail_mode),
        Some(TranscriptDetailMode::Full)
    );
}

#[test]
fn live_review_overlay_snapshot_includes_mode_and_browser_hints() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let area = Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);

    overlay.render(area, &mut buffer);

    let row = |y| {
        let mut text = String::new();
        for x in area.x..area.right() {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.trim_end().to_string()
    };
    assert_snapshot!(
        format!(
            "{}\n{}\n{}",
            overlay.view.title,
            row(area.bottom() - 3),
            row(area.bottom() - 2)
        ),
        @r"
    T R A N S C R I P T · R E V I E W
     ↑/↓ to scroll   pgup/pgdn to page   home/end to jump
     q close   v detail   [/] review items"
    );
}
