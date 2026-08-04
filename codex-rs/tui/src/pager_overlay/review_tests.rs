//! Inline chrome and key ownership share the live browser but not historical-preview behavior.

use super::*;
use crate::key_hint::plain;
use pretty_assertions::assert_eq;

#[derive(Debug)]
struct Output;

impl HistoryCell for Output {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        vec!["summary".into()]
    }
    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec!["first retained line".into(), "last retained line".into()]
    }
    fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.raw_lines()
    }
}

fn paint(overlay: &mut TranscriptOverlay) -> String {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 60, /*height*/ 10,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    buffer
        .content()
        .chunks(usize::from(area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn live_review_and_full_rendering_leave_historical_previews_unchanged() {
    let cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(Output)];
    let keymap = RuntimeKeymap::defaults().pager;
    let mut historical = TranscriptOverlay::new(cells.clone(), keymap.clone());
    let historical_before = paint(&mut historical);
    historical.handle_key(KeyCode::Char('v').into());
    assert_eq!(paint(&mut historical), historical_before);
    assert!(historical_before.contains("last retained line"));
    assert!(!historical_before.contains("REVIEW"));

    let Overlay::Transcript(mut live) = Overlay::new_review_transcript(cells, keymap) else {
        panic!("transcript")
    };
    let review = paint(&mut live);
    assert!(review.contains("R E V I E W"));
    assert!(review.contains("summary"));
    assert!(!review.contains("last retained line"));
    live.handle_key(KeyCode::Char('v').into());
    let full = paint(&mut live);
    assert!(full.contains("F U L L"));
    assert!(full.contains("last retained line"));
    // Snapshot the complete content area; headers/hints are covered independently at narrow widths.
    let content = |screen: &str| {
        screen
            .lines()
            .skip(1)
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    };
    insta::assert_snapshot!(format!("Review:\n{}\nFull:\n{}", content(&review), content(&full)), @"
    Review:
    summary
    Full:
    first retained line
    last retained line
    ");
}

#[test]
fn same_length_label_refresh_preserves_review_navigation() {
    #[derive(Debug)]
    struct ReviewOutput(&'static str);

    impl HistoryCell for ReviewOutput {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            vec![self.0.into()]
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.display_lines(u16::MAX)
        }

        fn transcript_navigation_kind(
            &self,
        ) -> Option<crate::history_cell::TranscriptNavigationKind> {
            Some(crate::history_cell::TranscriptNavigationKind::Commentary)
        }
    }

    let cells: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(ReviewOutput("first")),
        Arc::new(ReviewOutput("second")),
        Arc::new(ReviewOutput("third")),
    ];
    let Overlay::Transcript(mut live) =
        Overlay::new_review_transcript(cells, RuntimeKeymap::defaults().pager)
    else {
        panic!("transcript")
    };
    live.view.jump_to_entry(&live.cells, /*index*/ 0);
    paint(&mut live);
    live.handle_key(KeyCode::Char(']').into());
    paint(&mut live);
    live.replace_cells(vec![
        Arc::new(ReviewOutput("first renamed")),
        Arc::new(ReviewOutput("second")),
        Arc::new(ReviewOutput("third")),
    ]);
    paint(&mut live);
    live.handle_key(KeyCode::Char(']').into());
    let screen = paint(&mut live);
    assert_eq!(screen.lines().nth(/*n*/ 1).map(str::trim), Some("second"));
}

#[test]
fn fixed_browser_keys_precede_pager_bindings_but_not_search() {
    let mut keymap = RuntimeKeymap::defaults().pager;
    keymap.scroll_down = vec![plain(KeyCode::Char('v'))];
    keymap.find = vec![plain(KeyCode::Char(']'))];
    let Overlay::Transcript(mut live) =
        Overlay::new_review_transcript(vec![Arc::new(Output)], keymap)
    else {
        panic!("transcript")
    };
    paint(&mut live);
    live.handle_key(KeyCode::Char('v').into());
    assert!(live.is_detailed());
    live.handle_key(KeyCode::Char(']').into());
    assert!(!live.is_search_active());
    live.begin_search();
    live.handle_key(KeyCode::Char('v').into());
    assert!(live.is_detailed());
    assert!(live.is_search_active());
}
