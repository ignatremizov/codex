use super::*;
use pretty_assertions::assert_eq;

#[test]
fn legacy_annotated_wrapping_keeps_existing_logical_ranges_and_copy_policy() {
    let mut source = HyperlinkLine::new(Line::from("alpha beta"));
    let mut logical = LogicalLineSource::from_line(&source.line);
    logical.copy_as_prose = true;
    logical.right_reserve = 2;
    source.source = Some(logical.clone());
    source.hyperlinks.push(TerminalHyperlink::web(
        6..10,
        "https://example.com/beta".into(),
    ));
    let actual = remap_wrapped_line(&source, vec!["> alpha".into(), "  beta".into()]);
    let expected = vec![
        HyperlinkLine {
            line: "> alpha".into(),
            hyperlinks: Vec::new(),
            source: Some(logical.wrapped(0..5, /*prefix_bytes*/ 2)),
        },
        HyperlinkLine {
            line: "  beta".into(),
            hyperlinks: vec![TerminalHyperlink::web(
                2..6,
                "https://example.com/beta".into(),
            )],
            source: Some(logical.wrapped(6..10, /*prefix_bytes*/ 2)),
        },
    ];
    // HyperlinkLine's visual equality deliberately ignores logical copy provenance.
    assert_eq!(actual, expected);
    assert_eq!(
        actual.iter().map(|row| &row.source).collect::<Vec<_>>(),
        expected.iter().map(|row| &row.source).collect::<Vec<_>>(),
    );

    let blank_source = LogicalLineSource::from_line(&Line::from("  "));
    let blank = HyperlinkLine {
        line: "  ".into(),
        hyperlinks: Vec::new(),
        source: Some(blank_source.clone()),
    };
    let actual = remap_wrapped_line(&blank, vec![Line::default()]);
    assert_eq!(actual[0].source, Some(blank_source));
}

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
