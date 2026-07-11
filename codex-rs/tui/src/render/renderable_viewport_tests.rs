use super::*;
use pretty_assertions::assert_eq;

struct VirtualRows;

impl Renderable for VirtualRows {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_with_offset(area, buf, /*scroll_offset*/ 0);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        u16::MAX
    }
    fn desired_height_usize(&self, _width: u16) -> usize {
        70_000
    }
    fn layout_revision(&self) -> Option<u64> {
        Some(7)
    }
    fn render_with_offset(&self, area: Rect, buf: &mut Buffer, scroll_offset: usize) {
        Widget::render(
            Paragraph::new(
                (scroll_offset..scroll_offset + usize::from(area.height))
                    .map(|row| Line::from(row.to_string()))
                    .collect::<Vec<_>>(),
            ),
            area,
            buf,
        );
    }
}

#[test]
fn wrappers_forward_logical_offsets_heights_and_revisions() {
    let source = VirtualRows;
    let wrappers = [
        RenderableItem::Borrowed(&source),
        RenderableItem::Owned(Box::new(Some(Arc::new(VirtualRows)))),
    ];
    for wrapper in wrappers {
        assert_eq!(
            (
                wrapper.desired_height_usize(/*width*/ 8),
                wrapper.layout_revision()
            ),
            (70_000, Some(7))
        );
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 8, /*height*/ 2,
        );
        let mut actual = Buffer::empty(area);
        let mut expected = Buffer::empty(area);
        wrapper.render_with_offset(area, &mut actual, /*scroll_offset*/ 65_536);
        Widget::render(
            Paragraph::new(vec![Line::from("65536"), Line::from("65537")]),
            area,
            &mut expected,
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn insets_clip_at_the_actual_child_end_and_saturate_legacy_height() {
    let child = InsetRenderable::new(
        Box::new(VirtualRows) as Box<dyn Renderable>,
        Insets::tlbr(
            /*top*/ 2, /*left*/ 1, /*bottom*/ 3, /*right*/ 1,
        ),
    );
    assert_eq!(
        (
            child.desired_height(/*width*/ 8),
            child.desired_height_usize(/*width*/ 8),
            child.layout_revision()
        ),
        (u16::MAX, 70_005, Some(7))
    );
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 8, /*height*/ 5,
    );
    for offset in [0, 1, 65_536, 70_001, 70_002, 70_005] {
        let mut actual = Buffer::empty(area);
        child.render_with_offset(area, &mut actual, offset);
        let mut expected = Buffer::empty(area);
        for row in 0..area.height {
            let logical = offset + usize::from(row);
            if (2..70_002).contains(&logical) {
                Widget::render(
                    Line::from((logical - 2).to_string()),
                    Rect::new(/*x*/ 1, row, /*width*/ 6, /*height*/ 1),
                    &mut expected,
                );
            }
        }
        assert_eq!(actual, expected, "offset={offset}");
    }
}

#[test]
fn fallback_uses_the_existing_direct_scroll_path_before_allocating() {
    struct Direct;
    impl Renderable for Direct {
        fn render(&self, _area: Rect, _buf: &mut Buffer) {
            panic!("scratch fallback");
        }
        fn desired_height(&self, _width: u16) -> u16 {
            100
        }
        fn render_scrolled(&self, area: Rect, buf: &mut Buffer, scroll_offset: u16) -> bool {
            Widget::render(Line::from(scroll_offset.to_string()), area, buf);
            true
        }
    }
    let direct = RenderableItem::Owned(Box::new(Some(Arc::new(Direct))));
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 5, /*height*/ 1,
    );
    for offset in [0, 80] {
        let mut actual = Buffer::empty(area);
        let mut expected = Buffer::empty(area);
        direct.render_with_offset(area, &mut actual, offset);
        Widget::render(Line::from(offset.to_string()), area, &mut expected);
        assert_eq!(actual, expected);
    }
}

#[test]
fn generic_paragraph_height_matches_its_reachable_fallback_range() {
    let paragraph = Paragraph::new(vec![Line::from("x"); 65_540]);
    assert_eq!(paragraph.desired_height(/*width*/ 1), u16::MAX);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 1, /*height*/ 2,
    );
    let mut actual = Buffer::empty(area);
    paragraph.render_with_offset(area, &mut actual, usize::from(u16::MAX) - 1);
    let mut expected = Buffer::empty(area);
    expected[(0, 0)].set_symbol("x");
    assert_eq!(actual, expected);
}

#[test]
fn large_patch_approval_height_saturates_instead_of_wrapping() {
    let change = crate::diff_model::FileChange::Add {
        content: "x\n".repeat(/*n*/ 65_540),
    };
    assert_eq!(
        (
            change.desired_height(/*width*/ 80),
            change.desired_height_usize(/*width*/ 80)
        ),
        (u16::MAX, usize::from(u16::MAX))
    );
}

#[test]
fn generic_fallback_preserves_hyperlink_cell_metadata() {
    use crate::terminal_hyperlinks::HyperlinkLine;
    use crate::terminal_hyperlinks::HyperlinkParagraph;
    use ratatui::style::Style;

    struct Linked(Vec<HyperlinkLine>);
    impl Renderable for Linked {
        fn desired_height(&self, width: u16) -> u16 {
            u16::try_from(HyperlinkParagraph::new(&self.0, Style::default()).line_count(width))
                .unwrap_or(u16::MAX)
        }
        fn render(&self, area: Rect, buf: &mut Buffer) {
            HyperlinkParagraph::new(&self.0, Style::default()).render(area, buf);
        }
    }
    let mut line = HyperlinkLine::new("prefix ".into());
    line.push_span("漢字link👩‍💻".into(), Some("https://example.com/semantic"));
    let linked = Linked(vec![line, HyperlinkLine::new("tail".into())]);
    let area = Rect::new(
        /*x*/ 2, /*y*/ 1, /*width*/ 8, /*height*/ 2,
    );
    let canvas = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 4,
    );
    let full_area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        /*width*/ 8,
        linked.desired_height(/*width*/ 8),
    );
    let mut full = Buffer::empty(full_area);
    linked.render(full_area, &mut full);
    let mut actual = Buffer::empty(canvas);
    linked.render_with_offset(area, &mut actual, /*scroll_offset*/ 1);
    let mut expected = Buffer::empty(canvas);
    for row in 0..area.height {
        for column in 0..area.width {
            expected[(area.x + column, area.y + row)] = full[(column, row + 1)].clone();
        }
    }
    assert_eq!(actual, expected);
    assert!(
        actual
            .content
            .iter()
            .any(|cell| cell.symbol().contains("https://example.com/semantic"))
    );
}

#[test]
fn generic_paragraph_block_keeps_clipped_scratch_buffer_behavior() {
    let paragraph = Paragraph::new(vec![
        Line::from("one"),
        Line::from("two"),
        Line::from("three"),
    ])
    .block(ratatui::widgets::Block::bordered());
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 10, /*height*/ 2,
    );
    let mut actual = Buffer::empty(area);
    paragraph.render_with_offset(area, &mut actual, /*scroll_offset*/ 1);
    let scratch_area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 10, /*height*/ 3,
    );
    let mut scratch = Buffer::empty(scratch_area);
    Widget::render(paragraph, scratch_area, &mut scratch);
    let mut expected = Buffer::empty(area);
    for row in 0..area.height {
        for column in 0..area.width {
            expected[(column, row)] = scratch[(column, row + 1)].clone();
        }
    }
    assert_eq!(actual, expected);
}
