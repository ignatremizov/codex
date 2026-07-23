//! Browser rendering and navigation operate on the same logical cells in both presentations.

use super::*;
use crate::history_cell::TranscriptNavigationKind;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Debug)]
struct ReviewCell {
    display: &'static str,
    full: &'static str,
    kind: Option<TranscriptNavigationKind>,
    full_reads: Arc<AtomicUsize>,
    activity: bool,
}

impl HistoryCell for ReviewCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.display.lines().map(Line::from).collect()
    }

    fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.full_reads.fetch_add(/*val*/ 1, Ordering::Relaxed);
        self.full.lines().map(Line::from).collect()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.full.lines().map(Line::from).collect()
    }

    fn transcript_navigation_kind(&self) -> Option<TranscriptNavigationKind> {
        self.kind
    }

    fn activity_ids(&self) -> Vec<String> {
        if self.activity {
            vec![self.display.to_owned()]
        } else {
            Vec::new()
        }
    }
}

fn cell(label: &'static str, kind: Option<TranscriptNavigationKind>) -> Arc<dyn HistoryCell> {
    Arc::new(ReviewCell {
        display: label,
        full: label,
        kind,
        full_reads: Arc::default(),
        activity: false,
    })
}

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
}

#[test]
fn held_group_navigation_uses_displayed_identities_and_rejoins_the_surviving_target() {
    for toggle in [false, true] {
        let mut cells = vec![
            cell("A", Some(TranscriptNavigationKind::UserMessage)),
            cell("B", Some(TranscriptNavigationKind::Commentary)),
            cell("C\ncontinued C", Some(TranscriptNavigationKind::Commentary)),
            cell("D", Some(TranscriptNavigationKind::AssistantOutput)),
        ];
        let mut view = TranscriptView::default();
        view.open_review_browser(HistoryRenderMode::Rich);
        view.jump_to_entry(&cells, /*index*/ 2);
        render(&mut view, &cells, /*width*/ 30, /*height*/ 2);
        view.scroll(&cells, /*rows*/ 1);
        let replacement = cell("X", Some(TranscriptNavigationKind::Commentary));
        view.replace_group(&cells, 1..3, &replacement);
        cells.splice(1..3, [replacement]);
        assert!(view.held_reading.is_some());
        if toggle {
            view.handle_review_key(key('v'), &cells);
            let rendered = text(&render(
                &mut view, &cells, /*width*/ 30, /*height*/ 2,
            ));
            assert!(rendered.contains("continued C"));
        }
        view.handle_review_key(key(']'), &cells);
        assert_eq!(
            view.review.and_then(|browser| browser.selected),
            Some(EntryKey::cell(&cells[2]))
        );
        assert!(view.held_reading.is_none());
    }
}

#[test]
fn browser_boundaries_rebuild_retired_group_layouts_without_losing_source_identity() {
    let mut cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(ReviewCell {
        display: "old group",
        full: "old group details",
        kind: None,
        full_reads: Arc::default(),
        activity: true,
    })];
    let mut view = TranscriptView::default();
    view.jump_to_entry(&cells, /*index*/ 0);
    let normal = text(&render(
        &mut view, &cells, /*width*/ 30, /*height*/ 4,
    ));
    assert!(normal.contains("+ Show details"));
    let key = EntryKey::cell(&cells[0]);
    let replacement = cell("new group", None);
    view.replace_group(&cells, 0..1, &replacement);
    cells[0] = replacement;
    view.open_review_browser(HistoryRenderMode::Rich);
    let review = text(&render(
        &mut view, &cells, /*width*/ 30, /*height*/ 4,
    ));
    assert!(review.contains("old group"));
    assert!(!review.contains("Show details"));
    assert!(matches!(view.position, Position::Reading(anchor) if anchor.key == key));
    view.close_review_browser(HistoryRenderMode::Rich);
    assert_eq!(
        text(&render(
            &mut view, &cells, /*width*/ 30, /*height*/ 4
        )),
        normal
    );
}

#[test]
fn browser_boundaries_release_live_pins_and_refresh_same_revision_in_both_directions() {
    let mut view = TranscriptView::default();
    let live_key = Some(ActiveCellTranscriptKey {
        cacheable: true,
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    });
    for browser in [false, true, false] {
        if browser {
            view.open_review_browser(HistoryRenderMode::Rich);
        } else {
            view.close_review_browser(HistoryRenderMode::Rich);
        }
        let label = if browser { "Review live" } else { "Owned live" };
        view.sync_live_tail(
            /*width*/ 30,
            live_key,
            |_| Some(vec![HyperlinkLine::from(label)]),
        );
        view.jump_to_entry(&[], /*index*/ 0);
        let rendered = text(&render(
            &mut view,
            &[],
            /*width*/ 30,
            /*height*/ 2,
        ));
        view.scroll(&[], /*rows*/ 0);
        assert!(view.held_reading.is_some());
        assert_eq!(rendered.trim_end(), label);
    }
}

#[test]
fn retired_group_anchor_retains_a_visible_live_tail_in_each_browser_presentation() {
    let mut cells = vec![cell("retired group", None)];
    let mut view = TranscriptView::default();
    let live_key = Some(ActiveCellTranscriptKey {
        cacheable: true,
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    });
    view.sync_live_tail(/*width*/ 30, live_key, |_| {
        Some(vec![HyperlinkLine::from("owned live")])
    });
    view.jump_to_entry(&cells, /*index*/ 0);
    render(&mut view, &cells, /*width*/ 30, /*height*/ 8);
    let replacement = cell("current group", None);
    view.replace_group(&cells, 0..1, &replacement);
    cells[0] = replacement;
    for label in ["review live", "full live", "owned live"] {
        match label {
            "review live" => view.open_review_browser(HistoryRenderMode::Rich),
            "full live" => {
                view.handle_review_key(key('v'), &cells);
            }
            _ => view.close_review_browser(HistoryRenderMode::Rich),
        }
        view.sync_live_tail(
            /*width*/ 30,
            live_key,
            |_| Some(vec![HyperlinkLine::from(label)]),
        );
        let rendered = text(&render(
            &mut view, &cells, /*width*/ 30, /*height*/ 8,
        ));
        assert!(rendered.contains("retired group"));
        assert!(rendered.contains(label));
        assert!(!rendered.contains("current group"));
    }
}

#[test]
fn review_uses_display_without_requesting_full_output_and_toggle_keeps_the_anchor() {
    let reads = Arc::new(AtomicUsize::new(/*v*/ 0));
    let cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(ReviewCell {
        display: "Explored 3 files",
        full: "file one\nfile two\nfile three",
        kind: None,
        full_reads: Arc::clone(&reads),
        activity: false,
    })];
    let mut view = TranscriptView::default();
    view.open_review_browser(HistoryRenderMode::Rich);
    let review = text(&render(
        &mut view, &cells, /*width*/ 30, /*height*/ 4,
    ));
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    view.handle_review_key(key('v'), &cells);
    let full = text(&render(
        &mut view, &cells, /*width*/ 30, /*height*/ 4,
    ));
    assert!(reads.load(Ordering::Relaxed) > 0);
    view.handle_review_key(key('v'), &cells);
    let restored = text(&render(
        &mut view, &cells, /*width*/ 30, /*height*/ 4,
    ));
    assert_eq!(restored, review);
    insta::assert_snapshot!(format!("Review:\n{}\nFull:\n{}", review.trim_end(), full.trim_end()), @"
    Review:
    Explored 3 files
    Full:
    file one
    file two
    file three
    ");
}

#[test]
fn all_review_kinds_are_chronological_tools_are_skipped_and_navigation_does_not_wrap() {
    let cells = vec![
        cell("user", Some(TranscriptNavigationKind::UserMessage)),
        cell("tool", None),
        cell("commentary", Some(TranscriptNavigationKind::Commentary)),
        cell("final", Some(TranscriptNavigationKind::AssistantOutput)),
        cell("patch", Some(TranscriptNavigationKind::Patch)),
    ];
    let mut view = TranscriptView::default();
    view.open_review_browser(HistoryRenderMode::Rich);
    render(&mut view, &cells, /*width*/ 30, /*height*/ 3);
    view.jump_to_entry(&cells, /*index*/ 0);
    render(&mut view, &cells, /*width*/ 30, /*height*/ 3);
    for index in [0, 2, 3, 4, 4] {
        view.handle_review_key(key(']'), &cells);
        assert_eq!(
            view.review.and_then(|browser| browser.selected),
            Some(EntryKey::cell(&cells[index]))
        );
        render(&mut view, &cells, /*width*/ 30, /*height*/ 3);
    }
    for index in [3, 2, 0, 0] {
        view.handle_review_key(key('['), &cells);
        assert_eq!(
            view.review.and_then(|browser| browser.selected),
            Some(EntryKey::cell(&cells[index]))
        );
        render(&mut view, &cells, /*width*/ 30, /*height*/ 3);
    }
    view.scroll(&cells, /*rows*/ 1);
    assert!(view.review.and_then(|browser| browser.selected).is_none());
    view.jump_to_entry(&cells, /*index*/ 1);
    view.handle_review_key(key(']'), &cells);
    assert_eq!(
        view.review.and_then(|browser| browser.selected),
        Some(EntryKey::cell(&cells[2]))
    );
}

#[test]
fn target_identity_survives_prepend_mode_change_and_consolidation_but_not_replacement() {
    let mut cells = vec![
        cell("commentary", Some(TranscriptNavigationKind::Commentary)),
        cell("tail", None),
    ];
    let mut view = TranscriptView::default();
    view.open_review_browser(HistoryRenderMode::Rich);
    render(&mut view, &cells, /*width*/ 30, /*height*/ 5);
    view.handle_review_key(key(']'), &cells);
    let selected = view.review.and_then(|browser| browser.selected);
    cells.insert(/*index*/ 0, cell("older", None));
    view.history_loaded(&cells, 0..1);
    render(&mut view, &cells, /*width*/ 20, /*height*/ 5);
    view.handle_review_key(key('v'), &cells);
    assert_eq!(view.review.and_then(|browser| browser.selected), selected);
    let replacement = cell(
        "complete answer",
        Some(TranscriptNavigationKind::AssistantOutput),
    );
    view.replace_range(&cells, 1..2, &replacement);
    cells.splice(1..2, [Arc::clone(&replacement)]);
    assert_eq!(
        view.review.and_then(|browser| browser.selected),
        Some(EntryKey::cell(&replacement))
    );
    cells.push(cell(
        "append does not select itself",
        Some(TranscriptNavigationKind::UserMessage),
    ));
    render(&mut view, &cells, /*width*/ 20, /*height*/ 5);
    assert_eq!(
        view.review.and_then(|browser| browser.selected),
        Some(EntryKey::cell(&replacement))
    );
    let tool = cell("not a target", None);
    view.replace_range(&cells, 1..2, &tool);
    assert!(view.review.and_then(|browser| browser.selected).is_none());
}

#[test]
fn browser_keys_are_local_and_pre_layout_is_a_noop() {
    let cells = vec![cell("user", Some(TranscriptNavigationKind::UserMessage))];
    let mut view = TranscriptView::default();
    assert!(view.handle_review_key(key('v'), &cells).is_none());
    view.open_review_browser(HistoryRenderMode::Rich);
    for character in ['v', '[', ']'] {
        assert!(view.handle_review_key(key(character), &cells).is_some());
    }
    assert_eq!(view.review_mode(), Some(ReviewMode::Review));
    assert!(view.review.and_then(|browser| browser.selected).is_none());
    render(&mut view, &cells, /*width*/ 20, /*height*/ 3);
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SUPER,
        KeyModifiers::SHIFT,
    ] {
        assert!(!view.owns_review_key(KeyEvent::new(KeyCode::Char(']'), modifiers)));
    }
    assert_eq!(
        view.owns_review_key(KeyEvent::new(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        cfg!(windows),
    );
    assert!(!view.owns_review_key(KeyEvent::new_with_kind(
        KeyCode::Char('v'),
        KeyModifiers::NONE,
        KeyEventKind::Release
    )));
    view.begin_search();
    assert!(view.handle_review_key(key('v'), &cells).is_none());
    view.close_review_browser(HistoryRenderMode::Rich);
    assert!(!view.owns_review_key(key(']')));
}

#[test]
fn highlight_confirmation_requires_current_painted_content_not_a_separator() {
    let cells = vec![
        cell("first", None),
        cell("selected", Some(TranscriptNavigationKind::UserMessage)),
    ];
    let mut view = TranscriptView::default();
    render(&mut view, &cells, /*width*/ 20, /*height*/ 1);
    view.jump_to_entry(&cells, /*index*/ 1);
    view.scroll(&cells, /*rows*/ -1);
    view.set_highlight(Some(1));
    assert!(!view.highlighted_content_is_drawn(&cells, /*index*/ 1));
    // The leading separator occupies this entire one-row viewport.
    render(&mut view, &cells, /*width*/ 20, /*height*/ 1);
    assert!(!view.highlighted_content_is_drawn(&cells, /*index*/ 1));
    view.scroll(&cells, /*rows*/ 1);
    render(&mut view, &cells, /*width*/ 20, /*height*/ 1);
    assert!(view.highlighted_content_is_drawn(&cells, /*index*/ 1));
    view.prepare_width(/*width*/ 12);
    assert!(!view.highlighted_content_is_drawn(&cells, /*index*/ 1));
    render(&mut view, &cells, /*width*/ 12, /*height*/ 2);
    view.set_highlight(Some(0));
    assert!(!view.highlighted_content_is_drawn(&cells, /*index*/ 0));
}

#[test]
fn mode_labels_and_hint_groups_fit_atomically() {
    let mut view = TranscriptView::default();
    view.open_review_browser(HistoryRenderMode::Rich);
    let labels = [80, 24, 6].map(|width| view.review_title(width).expect("title").to_string());
    let hints = [80, 32, 12]
        .map(|width| view.review_footer(width, "q").expect("footer").text.lines[0].to_string());
    insta::assert_snapshot!(format!("{}\n{}", labels.join("\n"), hints.join("\n")), @"
    T R A N S C R I P T · R E V I E W
    TRANSCRIPT · REVIEW
    REVIEW
    Review · q close · v detail · [ review prev · ] review next
    Review · q close · v detail
    Review
    ");
}
