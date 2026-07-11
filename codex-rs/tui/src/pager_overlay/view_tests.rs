use super::*;
use crate::pager_overlay::cached_rows::CachedRows;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct Probe {
    height: AtomicUsize,
    measures: AtomicUsize,
    revision: AtomicU64,
    dynamic: bool,
}

#[test]
fn content_height_counts_renderables() {
    let mut view = PagerView::new(
        vec![
            Box::new(CachedRows::new(vec!["a".into(); 2])),
            Box::new(CachedRows::new(vec!["b".into(); 3])),
        ],
        "T".into(),
        /*scroll_offset*/ 0,
        crate::keymap::RuntimeKeymap::defaults().pager,
    );
    assert_eq!(view.content_height(/*width*/ 80), 5);
}

impl Renderable for Probe {
    fn desired_height(&self, _width: u16) -> u16 {
        u16::try_from(self.height.load(Ordering::Relaxed)).unwrap_or(u16::MAX)
    }
    fn desired_height_usize(&self, _width: u16) -> usize {
        self.measures.fetch_add(/*val*/ 1, Ordering::Relaxed);
        self.height.load(Ordering::Relaxed)
    }
    fn layout_revision(&self) -> Option<u64> {
        self.dynamic.then(|| self.revision.load(Ordering::Relaxed))
    }
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_with_offset(area, buf, /*scroll_offset*/ 0);
    }
    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        Widget::render(
            Paragraph::new(
                (scroll_offset..scroll_offset + usize::from(area.height))
                    .map(|row| Line::from(format!("row {row}")))
                    .collect::<Vec<_>>(),
            ),
            area,
            buf,
        );
    }
}

#[test]
fn prefix_cache_reuses_stable_offscreen_layout_and_refreshes_dynamic_theme_and_width() {
    for dynamic in [false, true] {
        let first = Arc::new(Probe {
            height: AtomicUsize::new(100),
            measures: AtomicUsize::new(0),
            revision: AtomicU64::new(0),
            dynamic,
        });
        let mut view = PagerView::new(
            vec![
                Box::new(first.clone()),
                Box::new(CachedRows::new(vec!["tail".into(); 4])),
            ],
            "T".into(),
            usize::MAX,
            crate::keymap::RuntimeKeymap::defaults().pager,
        );
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 4,
        );
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let expected = buffer.clone();
        view.render(area, &mut buffer);
        assert_eq!(
            (&buffer, first.measures.load(Ordering::Relaxed)),
            (&expected, 1)
        );
        if dynamic {
            first.height.store(/*val*/ 200, Ordering::Relaxed);
            first.revision.store(/*val*/ 1, Ordering::Relaxed);
            view.scroll_offset = usize::MAX;
            view.render(area, &mut buffer);
            assert_eq!(first.measures.load(Ordering::Relaxed), 2);
            assert_eq!(view.scroll_offset, 202);
        }
        let before = first.measures.load(Ordering::Relaxed);
        crate::render::highlight::invalidate_render_cache();
        view.render(area, &mut buffer);
        assert_eq!(first.measures.load(Ordering::Relaxed), before + 1);
        let narrow = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 10, /*height*/ 4,
        );
        view.render(narrow, &mut buffer);
        assert_eq!(first.measures.load(Ordering::Relaxed), before + 2);
    }
}

#[test]
fn deep_static_rows_render_only_the_requested_viewport() {
    let rows = CachedRows::new(
        (0..65_540)
            .map(|row| Line::from(format!("row{row}")))
            .collect(),
    );
    assert_eq!(rows.desired_height_usize(/*width*/ 10), 65_540);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
        /*width*/ 10, /*height*/ 3,
    ))
    .expect("terminal");
    terminal
        .draw(|frame| {
            rows.render_with_offset(
                frame.area(),
                frame.buffer_mut(),
                /*scroll_offset*/ 65_536,
            );
        })
        .expect("draw");
    insta::assert_snapshot!(terminal.backend(), @r#"
    "row65536  "
    "row65537  "
    "row65538  "
    "#);

    let rows = CachedRows::new(vec![Line::from(format!("{}abcd", "x".repeat(65_536)))]);
    assert_eq!(rows.desired_height_usize(/*width*/ 1), 65_540);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 1, /*height*/ 3,
    );
    let mut actual = Buffer::empty(area);
    rows.render_with_offset(area, &mut actual, /*scroll_offset*/ 65_536);
    let mut expected = Buffer::empty(area);
    for (row, symbol) in ["a", "b", "c"].into_iter().enumerate() {
        expected[(0, row as u16)].set_symbol(symbol);
    }
    assert_eq!(actual, expected);

    assert_eq!(rows.desired_height_usize(/*width*/ 2), 32_770);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 2, /*height*/ 2,
    );
    let mut actual = Buffer::empty(area);
    rows.render_with_offset(area, &mut actual, /*scroll_offset*/ 32_768);
    let mut expected = Buffer::empty(area);
    Widget::render(
        Paragraph::new(vec![Line::from("ab"), Line::from("cd")]),
        area,
        &mut expected,
    );
    assert_eq!(actual, expected);
}

#[test]
fn exact_fit_and_blank_rows_use_the_real_layout_before_viewport_rendering() {
    let mut view = PagerView::new(
        vec![Box::new(CachedRows::new(vec![
            "one".into(),
            Line::default(),
            "end".into(),
        ]))],
        "T".into(),
        usize::MAX,
        crate::keymap::RuntimeKeymap::defaults().pager,
    );
    let area = Rect::new(
        /*x*/ 2, /*y*/ 1, /*width*/ 12, /*height*/ 5,
    );
    let canvas = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 16, /*height*/ 7,
    );
    let mut actual = Buffer::empty(canvas);
    view.render(area, &mut actual);
    assert_eq!(view.scroll_offset, 0);
    let mut expected = actual.clone();
    let content = view.content_area(area);
    Clear.render(content, &mut expected);
    Widget::render(
        Paragraph::new(vec![Line::from("one"), Line::default(), Line::from("end")]),
        content,
        &mut expected,
    );
    assert_eq!(actual, expected);
}
