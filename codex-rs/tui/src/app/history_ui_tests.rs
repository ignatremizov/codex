use super::*;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use pretty_assertions::assert_eq;
use ratatui::text::Line;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[test]
fn desktop_thread_opened_history_snapshot() {
    let cell = history_cell::new_info_event(
        DESKTOP_THREAD_OPENED_MESSAGE.to_string(),
        /*hint*/ None,
    );

    insta::assert_snapshot!("desktop_thread_opened_history", render_cell(&cell));
}

#[test]
fn desktop_thread_open_error_history_snapshot() {
    let cell = history_cell::new_error_event(desktop_thread_open_error_message("launch failed"));

    insta::assert_snapshot!("desktop_thread_open_error_history", render_cell(&cell));
}

fn render_cell(cell: &impl HistoryCell) -> String {
    let lines = cell.display_lines(/*width*/ 80);
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug)]
struct CountingHistoryCell {
    renders: Arc<AtomicUsize>,
}

impl HistoryCell for CountingHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        vec![Line::from("prepared history")]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.display_lines(u16::MAX)
    }
}

#[tokio::test]
async fn insertion_prepares_lines_once_for_normal_and_buffered_history() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let renders = Arc::new(AtomicUsize::new(0));
    app.insert_history_cell(
        &mut tui,
        Box::new(CountingHistoryCell {
            renders: Arc::clone(&renders),
        }),
    );
    assert_eq!(renders.load(Ordering::Relaxed), 1);
    assert_eq!(
        app.last_rendered_history_tail
            .as_ref()
            .expect("history tail")
            .lines,
        vec![crate::terminal_hyperlinks::HyperlinkLine::new(Line::from(
            "prepared history"
        ))]
    );

    app.begin_initial_history_replay_buffer();
    app.insert_history_cell(
        &mut tui,
        Box::new(CountingHistoryCell {
            renders: Arc::clone(&renders),
        }),
    );
    assert_eq!(renders.load(Ordering::Relaxed), 2);
    Ok(())
}

#[tokio::test]
async fn deferred_thread_switch_insertion_does_not_render_cells() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let renders = Arc::new(AtomicUsize::new(0));
    app.begin_thread_switch_history_replay_buffer(/*visible_rows*/ 24);
    app.insert_history_cell(
        &mut tui,
        Box::new(CountingHistoryCell {
            renders: Arc::clone(&renders),
        }),
    );
    assert_eq!(renders.load(Ordering::Relaxed), 0);
    assert_eq!(app.transcript_cells.len(), 1);
    assert!(app.last_rendered_history_tail.is_none());
    Ok(())
}
