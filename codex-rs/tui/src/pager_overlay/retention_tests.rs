use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::ActiveCellTranscriptKey;
use super::HistoryCell;
use super::HyperlinkLine;
use super::TranscriptFlavor;
use super::TranscriptOverlay;
use super::transcript::TranscriptDetailMode;
use crate::keymap::RuntimeKeymap;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

#[derive(Debug)]
struct CountingCell {
    calls: Arc<AtomicUsize>,
    text: String,
}

impl HistoryCell for CountingCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        vec![Line::from(self.text.clone())]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from(self.text.clone())]
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.display_lines(width);
        lines.push(Line::from("full audit detail"));
        lines
    }
}

fn cells(count: usize, calls: &Arc<AtomicUsize>) -> Vec<Arc<dyn HistoryCell>> {
    (0..count)
        .map(|index| {
            Arc::new(CountingCell {
                calls: Arc::clone(calls),
                text: format!("cell-{index} 界 e\u{301}"),
            }) as Arc<dyn HistoryCell>
        })
        .collect()
}

fn overlay(count: usize, calls: &Arc<AtomicUsize>) -> TranscriptOverlay {
    TranscriptOverlay::new(
        cells(count, calls),
        RuntimeKeymap::defaults().pager,
        TranscriptFlavor::LiveReviewBrowser,
    )
}

fn draw(overlay: &mut TranscriptOverlay) -> Buffer {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 40, /*height*/ 12,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    buffer
}

#[test]
fn append_to_warmed_full_window_only_materializes_the_new_cell() {
    for detail_mode in [TranscriptDetailMode::Review, TranscriptDetailMode::Full] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut overlay = overlay(/*count*/ 256, &calls);
        if detail_mode == TranscriptDetailMode::Full {
            overlay.browser.toggle_detail_mode();
            overlay.rebuild_renderables();
        }
        draw(&mut overlay);
        assert_eq!(calls.load(Ordering::Relaxed), 256);

        overlay.insert_cell(cells(/*count*/ 1, &calls).remove(/*index*/ 0));
        let actual = draw(&mut overlay);

        assert_eq!(calls.load(Ordering::Relaxed), 257);
        assert_eq!((overlay.render_start, overlay.render_end), (1, 257));
        assert!(overlay.view.is_scrolled_to_bottom());
        let mut reopened = TranscriptOverlay::new(
            overlay.cells.clone(),
            RuntimeKeymap::defaults().pager,
            TranscriptFlavor::LiveReviewBrowser,
        );
        if detail_mode == TranscriptDetailMode::Full {
            reopened.browser.toggle_detail_mode();
            reopened.rebuild_renderables();
        }
        assert_eq!(actual, draw(&mut reopened));
    }
}

#[test]
fn consolidation_after_older_window_does_not_invalidate_layout_or_rows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 600, &calls);
    overlay.show_loaded_start();
    let before = draw(&mut overlay);
    let layout = overlay.view.chunk_bottoms.clone();
    let measured = calls.load(Ordering::Relaxed);

    overlay.consolidate_cells(500..510, cells(/*count*/ 1, &calls).remove(/*index*/ 0));

    assert_eq!(overlay.view.chunk_bottoms, layout);
    assert_eq!(draw(&mut overlay), before);
    assert_eq!(calls.load(Ordering::Relaxed), measured);
}

#[test]
fn consolidation_before_window_preserves_identity_and_viewport_anchor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 600, &calls);
    overlay.view.scroll_offset = 10;
    let before = draw(&mut overlay);
    let measured = calls.load(Ordering::Relaxed);

    overlay.consolidate_cells(0..10, cells(/*count*/ 1, &calls).remove(/*index*/ 0));

    assert_eq!(draw(&mut overlay), before);
    assert_eq!(calls.load(Ordering::Relaxed), measured);
}

#[test]
fn consolidation_inside_window_materializes_only_replacement() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 256, &calls);
    overlay.view.scroll_offset = 300;
    draw(&mut overlay);
    let anchor = overlay.global_viewport_anchor().unwrap();

    overlay.consolidate_cells(100..110, cells(/*count*/ 1, &calls).remove(/*index*/ 0));
    draw(&mut overlay);

    assert_eq!(calls.load(Ordering::Relaxed), 257);
    assert_eq!(
        overlay.global_viewport_anchor(),
        Some(super::ViewportAnchor {
            chunk_index: anchor.chunk_index - 9,
            row_offset: anchor.row_offset,
        })
    );
}

#[test]
fn detail_changes_do_not_reuse_review_rows_as_full_audit_rows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 256, &calls);
    let review = draw(&mut overlay);

    overlay.browser.toggle_detail_mode();
    overlay.rebuild_renderables();
    let full = draw(&mut overlay);

    assert_eq!(calls.load(Ordering::Relaxed), 512);
    assert_ne!(full, review);
    draw(&mut overlay);
    assert_eq!(calls.load(Ordering::Relaxed), 512);
}

#[test]
fn overlapping_navigation_reuses_rows_even_when_first_cell_spacing_changes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 600, &calls);
    overlay.show_loaded_start();
    draw(&mut overlay);
    assert_eq!(calls.load(Ordering::Relaxed), 256);

    assert!(overlay.show_next_window());
    draw(&mut overlay);
    assert_eq!(calls.load(Ordering::Relaxed), 480);

    // Only the overlap survives; this deliberately does not retain an off-window LRU.
    assert!(overlay.show_previous_window());
    draw(&mut overlay);
    assert_eq!(calls.load(Ordering::Relaxed), 704);
    assert_eq!((overlay.render_start, overlay.render_end), (0, 256));
}

#[test]
fn prepend_and_identity_replacement_retain_warmed_window() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 600, &calls);
    overlay.view.scroll_offset = 10;
    let before = draw(&mut overlay);
    let measured = calls.load(Ordering::Relaxed);

    overlay.prepend(cells(/*count*/ 20, &calls), /*width*/ 40);
    assert_eq!(draw(&mut overlay), before);
    overlay.replace_cells(overlay.cells.clone());
    assert_eq!(draw(&mut overlay), before);
    assert_eq!(calls.load(Ordering::Relaxed), measured);
}

#[test]
fn offscreen_tail_defers_updates_and_materializes_latest_revision_on_return() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 256, &calls);
    let key = ActiveCellTranscriptKey {
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    };
    overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
        Some(vec![HyperlinkLine::from("initial tail")])
    });
    draw(&mut overlay);
    overlay.view.scroll_offset = 0;
    let before = draw(&mut overlay);

    for revision in 2..5 {
        overlay.sync_live_tail(
            /*width*/ 40,
            Some(ActiveCellTranscriptKey { revision, ..key }),
            |_| panic!("offscreen tail must not be materialized"),
        );
        assert_eq!(draw(&mut overlay), before);
    }
    assert_eq!(overlay.live_tail_key.map(|key| key.revision), Some(1));
    overlay.show_loaded_end();
    overlay.sync_live_tail(
        /*width*/ 40,
        Some(ActiveCellTranscriptKey { revision: 4, ..key }),
        |_| Some(vec![HyperlinkLine::from("latest tail")]),
    );
    draw(&mut overlay);
    assert_eq!(overlay.live_tail_key.map(|key| key.revision), Some(4));
    assert_eq!(calls.load(Ordering::Relaxed), 256);
}

#[test]
fn absent_offscreen_tail_is_not_materialized_until_visible() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 256, &calls);
    overlay.view.scroll_offset = 0;
    draw(&mut overlay);
    let key = ActiveCellTranscriptKey {
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    };
    overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
        panic!("loaded end in window does not mean tail is visible")
    });
    assert_eq!(overlay.live_tail_key, None);

    overlay.show_loaded_end();
    let mut computed = false;
    overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
        computed = true;
        Some(vec![HyperlinkLine::from("new tail")])
    });
    assert!(computed);
    assert_eq!(overlay.view.renderables.len(), 257);
}

#[test]
fn partially_visible_tall_tail_updates_without_bottom_following() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut overlay = overlay(/*count*/ 1, &calls);
    let key = ActiveCellTranscriptKey {
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    };
    overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
        Some(vec![HyperlinkLine::from("initial"); 100])
    });
    draw(&mut overlay);
    overlay.view.scroll_offset = 3;
    draw(&mut overlay);
    assert!(!overlay.view.is_scrolled_to_bottom());

    let mut computed = false;
    overlay.sync_live_tail(
        /*width*/ 40,
        Some(ActiveCellTranscriptKey { revision: 2, ..key }),
        |_| {
            computed = true;
            Some(vec![HyperlinkLine::from("updated"); 100])
        },
    );
    assert!(computed);
    draw(&mut overlay);
    assert_eq!(overlay.view.scroll_offset, 3);
    assert_eq!(overlay.live_tail_key.map(|key| key.revision), Some(2));
}
