use super::make_test_app_with_channels;
use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::local_settings::LocalSettings;
use crate::render::highlight::syntax_theme_revision;
use crate::transcript_view::TranscriptView;
use codex_config::types::DiffBackgroundMode;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn diff_backgrounds_change_only_when_local_settings_are_adopted() {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let mut baseline = LocalSettings::from(&app.config);
    baseline.tui.diff_background = DiffBackgroundMode::Off;
    baseline.tui.diff_add_bg = None;
    baseline.tui.diff_del_bg = None;
    app.adopt_local_settings(baseline.clone());
    let revision = syntax_theme_revision();

    let mut config = app.config.clone();
    config.tui_diff_background = DiffBackgroundMode::Custom;
    config.tui_diff_add_bg = Some("#AABBCC".to_string());
    config.tui_diff_del_bg = Some("#DDEEFF".to_string());
    let mut expected = baseline;
    expected.tui.diff_background = DiffBackgroundMode::Custom;
    expected.tui.diff_add_bg = config.tui_diff_add_bg.clone();
    expected.tui.diff_del_bg = config.tui_diff_del_bg.clone();

    let converted = LocalSettings::from(&config);
    assert_eq!(
        (converted.clone(), syntax_theme_revision()),
        (expected.clone(), revision)
    );
    let reloaded = app.local_settings.reloaded(&config);
    assert_eq!(
        (reloaded, syntax_theme_revision()),
        (expected.clone(), revision)
    );

    app.adopt_local_settings(converted);
    assert_eq!(
        (app.local_settings.clone(), syntax_theme_revision()),
        (expected.clone(), revision.wrapping_add(1))
    );

    expected.tui.diff_add_bg = Some("#aabbcc".to_string());
    expected.tui.diff_del_bg = Some("#ddeeff".to_string());
    app.adopt_local_settings(expected.clone());
    assert_eq!(
        (app.local_settings.clone(), syntax_theme_revision()),
        (expected, revision.wrapping_add(1))
    );
}

#[derive(Debug, Default)]
struct CountingCell {
    renders: AtomicUsize,
}

impl HistoryCell for CountingCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.renders.fetch_add(/*val*/ 1, Ordering::Relaxed);
        vec!["cached transcript".into()]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec!["cached transcript".into()]
    }
}

#[tokio::test]
async fn adopted_diff_backgrounds_invalidate_the_transcript_layout_cache() {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let mut settings = app.local_settings.clone();
    settings.tui.diff_background = DiffBackgroundMode::Off;
    settings.tui.diff_add_bg = None;
    settings.tui.diff_del_bg = None;
    app.adopt_local_settings(settings.clone());

    let cell = Arc::new(CountingCell::default());
    let history: Vec<Arc<dyn HistoryCell>> = vec![cell.clone()];
    let mut view = TranscriptView::default();
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 40, /*height*/ 4,
    );
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer, &history);
    view.render(area, &mut buffer, &history);
    assert_eq!(cell.renders.load(Ordering::Relaxed), 1);
    let revision = syntax_theme_revision();

    settings.tui.diff_background = DiffBackgroundMode::Custom;
    settings.tui.diff_add_bg = Some("#123456".to_string());
    settings.tui.diff_del_bg = Some("#abcdef".to_string());
    app.adopt_local_settings(settings.clone());
    view.render(area, &mut buffer, &history);
    assert_eq!(
        (
            cell.renders.load(Ordering::Relaxed),
            syntax_theme_revision()
        ),
        (2, revision.wrapping_add(1))
    );

    settings.tui.diff_del_bg = Some("#ABCDEF".to_string());
    app.adopt_local_settings(settings);
    view.render(area, &mut buffer, &history);
    assert_eq!(
        (
            cell.renders.load(Ordering::Relaxed),
            syntax_theme_revision()
        ),
        (2, revision.wrapping_add(1))
    );
}
