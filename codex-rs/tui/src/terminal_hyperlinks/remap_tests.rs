use super::*;
use pretty_assertions::assert_eq;

#[test]
fn legacy_projection_preserves_grapheme_link_columns_and_destinations() {
    for (text, end) in [("A 👩‍💻 B", 4), ("A e\u{301} B", 3)] {
        let mut source = HyperlinkLine::new(Line::from(text));
        source.hyperlinks.push(TerminalHyperlink::web(
            2..end,
            "https://example.com/linked".into(),
        ));
        assert_eq!(
            remap_wrapped_line(&source, vec![source.line.clone()]),
            vec![source]
        );
    }
    for omitted in ["\u{301}", "\u{200d}"] {
        let mut source = HyperlinkLine::new(Line::from(format!("{omitted}A B")));
        source.hyperlinks.push(TerminalHyperlink::web(
            2..3,
            "https://example.com/after".into(),
        ));
        let mut expected = HyperlinkLine::new(Line::from("B"));
        expected.hyperlinks.push(TerminalHyperlink::web(
            0..1,
            "https://example.com/after".into(),
        ));
        assert_eq!(
            remap_wrapped_line(&source, vec!["A".into(), "B".into()]),
            vec![HyperlinkLine::new("A".into()), expected]
        );
    }
}

#[test]
fn matching_work_is_linear_for_large_whitespace_prefixes() {
    for matched in [false, true] {
        let source = format!("{}tail", " ".repeat(100_000));
        let rendered = if matched {
            "tail".to_string()
        } else {
            format!("{}b", " ".repeat(10_000))
        };
        let (result, inspected) = wrapped_fragment_match_impl(
            &rendered, &source, /*allow_source_whitespace_skip*/ true,
        );
        assert_eq!(result, matched.then_some((0, 100_000)));
        assert!(inspected <= 8 * (source.chars().count() + rendered.chars().count()));
    }
}
