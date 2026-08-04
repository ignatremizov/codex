use super::*;
use std::collections::HashMap;
use std::path::Path;

use codex_config::types::KeybindingSpec;
use codex_config::types::KeybindingsSpec;
use codex_config::types::TuiKeymap;
use codex_protocol::models::MessagePhase;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::chatwidget::ActiveCellTranscriptKey;
use crate::diff_model::FileChange;
use crate::history_cell;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::TranscriptNavigationKind;
use crate::history_cell::new_user_prompt;
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
        Arc::new(new_user_prompt(
            "user".to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
        Arc::new(AgentMarkdownCell::new_with_phase(
            "commentary".to_string(),
            Path::new("/tmp"),
            Some(MessagePhase::Commentary),
        )),
        Arc::new(PlainHistoryCell::new(vec!["tool status".into()])),
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

fn render_overlay_once(overlay: &mut TranscriptOverlay) {
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
}

fn row_text(buffer: &Buffer, area: Rect, y: u16) -> String {
    (area.x..area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
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
fn review_navigation_visits_turn_boundaries_commentary_and_patches() {
    let cells = review_cells();
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);

    assert_eq!(
        Some(0),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(1),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(3),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(4),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Next)
    );
    assert_eq!(
        Some(3),
        state.select_review_target(&cells, 0, TranscriptNavigationDirection::Previous)
    );
}

#[test]
fn unknown_phase_assistant_output_remains_a_review_target() {
    let cell = AgentMarkdownCell::new("assistant output".to_string(), Path::new("/tmp"));

    assert_eq!(
        Some(TranscriptNavigationKind::AssistantOutput),
        cell.transcript_navigation_kind()
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
fn prepending_history_rebases_selected_target() {
    let cells = review_cells();
    let mut state = TranscriptBrowserState::new(TranscriptFlavor::LiveReviewBrowser);
    let _ = state.select_review_target(&cells, 3, TranscriptNavigationDirection::Previous);

    state.prepend(/*insert_at*/ 1, /*added_cells*/ 2);

    assert_eq!(Some(5), state.selected_review_target());
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
fn live_review_overlay_renders_narrow_footer_without_percent() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let area = Rect::new(0, 0, 4, 10);
    let mut buffer = Buffer::empty(area);

    overlay.render(area, &mut buffer);

    let separator_y = area.y + area.height.saturating_sub(4);
    let separator = (area.x..area.right())
        .map(|x| buffer[(x, separator_y)].symbol())
        .collect::<String>();
    assert_snapshot!(
        format!("{}\n{separator}", overlay.view.title),
        @r"
    RE
    ────"
    );
}

#[tokio::test]
async fn live_overlay_handles_mode_navigation_and_manual_scroll() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    render_overlay_once(&mut overlay);

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
    assert_eq!(Some(0), overlay.selected_review_target());

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
fn backtrack_highlight_visibility_excludes_unstyled_top_inset() {
    let mut overlay = TranscriptOverlay::new(
        vec![
            Arc::new(PlainHistoryCell::new(
                (0..20)
                    .map(|line| format!("tool line {line}").into())
                    .collect(),
            )),
            Arc::new(new_user_prompt(
                "selected prompt".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        ],
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    overlay.set_highlight_cell(Some(1));
    let area = Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);

    let viewport_height = overlay
        .view
        .last_content_height
        .expect("render should establish viewport height");
    let highlight_top = overlay.view.chunk_bottoms[0];
    let highlight_content_top = highlight_top.saturating_add(1);
    overlay.view.scroll_offset = highlight_content_top.saturating_sub(viewport_height);
    overlay.render(area, &mut buffer);

    assert!(overlay.highlight_draw_pending());

    overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(1);
    overlay.render(area, &mut buffer);

    assert!(!overlay.highlight_draw_pending());
}

#[tokio::test]
async fn prepending_history_preserves_viewport_away_from_backtrack_highlight() {
    let mut overlay = TranscriptOverlay::new(
        vec![
            Arc::new(PlainHistoryCell::new(
                (0..30)
                    .map(|line| format!("current line {line}").into())
                    .collect(),
            )),
            Arc::new(new_user_prompt(
                "selected prompt".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        ],
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    overlay.set_highlight_cell(Some(1));
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let area = Rect::new(0, 0, 80, 12);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        )
        .expect("scroll to start");
    overlay.render(area, &mut buffer);
    assert!(overlay.highlight_draw_pending());
    let viewport_top_before = row_text(&buffer, area, /*y*/ 1);

    overlay.prepend(
        vec![Arc::new(PlainHistoryCell::new(
            (0..5)
                .map(|line| format!("older line {line}").into())
                .collect(),
        ))],
        area.width,
    );
    // `App::handle_older_history_page` re-applies the same logical selection after prepending.
    // That must not turn the unchanged selection into a fresh scroll request.
    overlay.set_highlight_cell(Some(2));
    overlay.render(area, &mut buffer);

    assert_eq!(row_text(&buffer, area, /*y*/ 1), viewport_top_before);
    assert!(overlay.highlight_draw_pending());
}

#[test]
fn rebuilding_taller_content_above_viewport_preserves_visible_row_anchor() {
    let visible_cell: Arc<dyn HistoryCell> = Arc::new(PlainHistoryCell::new(
        (0..30)
            .map(|line| format!("visible line {line}").into())
            .collect(),
    ));
    let selected_prompt: Arc<dyn HistoryCell> = Arc::new(new_user_prompt(
        "selected prompt".to_string(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    let mut overlay = TranscriptOverlay::new(
        vec![
            Arc::new(PlainHistoryCell::new(vec!["short heading".into()])),
            Arc::clone(&visible_cell),
            Arc::clone(&selected_prompt),
        ],
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    overlay.set_highlight_cell(Some(2));
    let area = Rect::new(0, 0, 80, 12);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let visible_cell_top = overlay.view.chunk_bottoms[0];
    overlay.view.scroll_offset = visible_cell_top.saturating_add(10);
    overlay.render(area, &mut buffer);
    let viewport_top_before = row_text(&buffer, area, /*y*/ 1);
    assert!(viewport_top_before.starts_with("visible line "));
    assert!(overlay.highlight_draw_pending());

    overlay.replace_cells(vec![
        Arc::new(PlainHistoryCell::new(
            (0..8)
                .map(|line| format!("expanded heading {line}").into())
                .collect(),
        )),
        visible_cell,
        selected_prompt,
    ]);
    overlay.render(area, &mut buffer);

    assert_eq!(row_text(&buffer, area, /*y*/ 1), viewport_top_before);
    assert!(overlay.highlight_draw_pending());
}

#[test]
fn removing_cell_before_viewport_preserves_visible_row_and_selection() {
    let heading: Arc<dyn HistoryCell> = Arc::new(PlainHistoryCell::new(vec!["heading".into()]));
    let removed: Arc<dyn HistoryCell> =
        Arc::new(PlainHistoryCell::new(vec!["hidden later".into()]));
    let visible_cell: Arc<dyn HistoryCell> = Arc::new(PlainHistoryCell::new(
        (0..30)
            .map(|line| format!("visible line {line}").into())
            .collect(),
    ));
    let selected_prompt: Arc<dyn HistoryCell> = Arc::new(new_user_prompt(
        "selected prompt".to_string(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    let mut overlay = TranscriptOverlay::new(
        vec![
            Arc::clone(&heading),
            removed,
            Arc::clone(&visible_cell),
            Arc::clone(&selected_prompt),
        ],
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    overlay.set_highlight_cell(Some(3));
    let area = Rect::new(0, 0, 80, 12);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let visible_cell_top = overlay.view.chunk_bottoms[1];
    overlay.view.scroll_offset = visible_cell_top.saturating_add(10);
    overlay.render(area, &mut buffer);
    let viewport_top_before = row_text(&buffer, area, /*y*/ 1);
    assert!(viewport_top_before.starts_with("visible line "));

    overlay.replace_cells(vec![heading, visible_cell, selected_prompt]);
    overlay.render(area, &mut buffer);

    assert_eq!(row_text(&buffer, area, /*y*/ 1), viewport_top_before);
    assert_eq!(overlay.highlight_cell, Some(2));
    assert!(overlay.highlight_draw_pending());
}

#[tokio::test]
async fn detail_toggle_preserves_pending_review_target_alignment() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    render_overlay_once(&mut overlay);

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
        )
        .expect("navigate");
    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        )
        .expect("toggle detail");

    assert_eq!(Some(0), overlay.selected_review_target());
    assert_eq!(Some(0), overlay.view.pending_align_chunk_top);
}

#[test]
fn detail_toggle_preserves_pending_backtrack_scroll() {
    let mut overlay = TranscriptOverlay::new(
        vec![
            Arc::new(PlainHistoryCell::new(
                (0..30)
                    .map(|line| format!("leading line {line}").into())
                    .collect(),
            )),
            Arc::new(new_user_prompt(
                "selected prompt".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        ],
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let area = Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);

    overlay.set_highlight_cell(Some(1));
    assert_eq!(overlay.view.pending_scroll_chunk, Some(1));
    overlay.toggle_detail_mode();

    assert_eq!(overlay.view.pending_scroll_chunk, Some(1));
    overlay.render(area, &mut buffer);
    assert!(!overlay.highlight_draw_pending());
}

#[tokio::test]
async fn consolidation_restores_pending_review_target_alignment() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    render_overlay_once(&mut overlay);

    for _ in 0..4 {
        overlay
            .handle_event(
                &mut tui,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            )
            .expect("navigate");
    }
    overlay.consolidate_cells(
        0..2,
        Arc::new(PlainHistoryCell::new(vec!["consolidated".into()])),
    );

    assert_eq!(Some(3), overlay.selected_review_target());
    assert_eq!(Some(3), overlay.view.pending_align_chunk_top);
}

#[tokio::test]
async fn same_length_cell_refresh_preserves_review_target() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    render_overlay_once(&mut overlay);
    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
        )
        .expect("navigate");

    overlay.replace_cells(review_cells());

    assert_eq!(Some(0), overlay.selected_review_target());
}

#[tokio::test]
async fn browser_actions_wait_for_initial_viewport_render() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");

    for character in ['v', '[', ']'] {
        overlay
            .handle_event(
                &mut tui,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            )
            .expect("handle browser action");
    }
    assert_eq!(TranscriptDetailMode::Review, overlay.browser.detail_mode());
    assert_eq!(None, overlay.selected_review_target());

    render_overlay_once(&mut overlay);
    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        )
        .expect("toggle detail");
    assert_eq!(TranscriptDetailMode::Full, overlay.browser.detail_mode());
}

#[test]
fn review_navigation_rejects_ctrl_or_alt_only_brackets() {
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        assert!(!is_review_navigation_char(
            KeyEvent::new(KeyCode::Char(']'), modifiers),
            ']'
        ));
    }
}

#[cfg(windows)]
#[test]
fn review_navigation_accepts_altgr_brackets() {
    assert!(is_review_navigation_char(
        KeyEvent::new(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        ']'
    ));
}

#[tokio::test]
async fn detail_toggle_invalidates_and_rebuilds_live_tail_in_new_mode() {
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
    overlay.sync_live_tail(
        /*width*/ 80,
        Some(key),
        |_| Some(vec!["review tail".into()]),
    );
    assert_eq!(overlay.view.renderables.len(), 1);
    assert_eq!(
        overlay.live_tail_key.map(|key| key.detail_mode),
        Some(TranscriptDetailMode::Review)
    );
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    render_overlay_once(&mut overlay);

    overlay
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        )
        .expect("toggle detail");
    assert!(overlay.view.renderables.is_empty());
    overlay.sync_live_tail(
        /*width*/ 80,
        Some(key),
        |_| Some(vec!["full tail".into()]),
    );

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
     q close   v detail   [ review prev   ] review next"
    );
}

#[test]
fn live_review_overlay_snapshot_keeps_pager_hints_during_backtrack() {
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        crate::keymap::RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    overlay.set_highlight_cell(Some(0));
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
        format!("{}\n{}", row(area.bottom() - 3), row(area.bottom() - 2)),
        @r"
     ↑/↓ to scroll   pgup/pgdn to page   home/end to jump
     q to quit   esc/← to edit prev   → to edit next   enter to edit message"
    );
}

#[test]
fn live_review_footer_renders_configured_chord_hint() {
    let mut config = TuiKeymap::default();
    config.pager.close = Some(KeybindingsSpec::One(KeybindingSpec("ctrl-x q".to_string())));
    let keymap = crate::keymap::RuntimeKeymap::from_config(&config).expect("valid pager chord");
    let mut overlay = TranscriptOverlay::new(
        review_cells(),
        keymap.pager,
        TranscriptFlavor::LiveReviewBrowser,
    );
    let area = Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);

    overlay.render(area, &mut buffer);

    let hint_row = (area.x..area.right())
        .map(|x| buffer[(x, area.bottom() - 2)].symbol())
        .collect::<String>();
    assert_snapshot!(
        hint_row.trim_end(),
        @" ctrl + x q close   v detail   [ review prev   ] review next"
    );
}
