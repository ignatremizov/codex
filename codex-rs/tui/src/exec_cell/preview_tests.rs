use super::*;
use crate::exec_cell::LiveCommandOutput;
use crate::terminal_hyperlinks::TerminalHyperlink;
use pretty_assertions::assert_eq;

fn text(lines: &[HyperlinkLine]) -> String {
    lines
        .iter()
        .map(|line| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn caps_preserve_head_tail_and_uncapped_source() {
    let source = (1..=8)
        .map(|n| HyperlinkLine::from(format!("line {n}")))
        .collect::<Vec<_>>();
    let uncapped = output_preview(
        source.clone(),
        /*width*/ 80,
        /*limit*/ 0,
        /*omitted_storage_lines*/ 0,
    );
    for limit in [8, 9] {
        let preview = output_preview(
            source.clone(),
            /*width*/ 80,
            limit,
            /*omitted_storage_lines*/ 0,
        );
        assert!(!preview.hidden);
        assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
            &preview.lines,
            &uncapped.lines,
        ));
    }
    let mut cases = Vec::new();
    for limit in [0, 1, 2, 5, 30, 50] {
        let preview = output_preview(
            source.clone(),
            /*width*/ 80,
            limit,
            /*omitted_storage_lines*/ 0,
        );
        assert_eq!(preview.hidden, limit != 0 && limit < source.len());
        assert_eq!(
            preview.lines.len(),
            if limit == 0 {
                8
            } else {
                limit.min(/*other*/ 8)
            }
        );
        cases.push(format!("limit {limit}\n{}", text(&preview.lines)));
    }
    insta::assert_snapshot!(cases.join("\n\n"), @r"
    limit 0
    line 1
    line 2
    line 3
    line 4
    line 5
    line 6
    line 7
    line 8

    limit 1
    … +8 rows (ctrl+t to view transcript)

    limit 2
    line 1
    … +7 rows (ctrl+t to view transcript)

    limit 5
    line 1
    line 2
    … +4 rows (ctrl+t to view transcript)
    line 7
    line 8

    limit 30
    line 1
    line 2
    line 3
    line 4
    line 5
    line 6
    line 7
    line 8

    limit 50
    line 1
    line 2
    line 3
    line 4
    line 5
    line 6
    line 7
    line 8
    ");
}

#[test]
fn narrow_url_and_unicode_rows_preserve_source_and_link_targets() {
    let url = format!("https://example.com/{}", "日本語".repeat(/*n*/ 20));
    let mut line = HyperlinkLine::from(url.clone());
    line.hyperlinks
        .push(TerminalHyperlink::web(0..line.width(), url));
    let full = output_preview(
        [line.clone()],
        /*width*/ 20,
        /*limit*/ 0,
        /*omitted_storage_lines*/ 0,
    );
    let preview = output_preview(
        [line],
        /*width*/ 20,
        /*limit*/ 5,
        /*omitted_storage_lines*/ 0,
    );
    assert!(full.lines.len() > 5);
    assert_eq!(preview.lines.len(), 5);
    assert_eq!(&preview.lines[..2], &full.lines[..2]);
    assert_eq!(&preview.lines[3..], &full.lines[full.lines.len() - 2..]);
    assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
        &preview.lines[..2],
        &full.lines[..2],
    ));
    assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
        &preview.lines[3..],
        &full.lines[full.lines.len() - 2..],
    ));
    assert!(preview.lines.iter().all(|row| row.width() <= 20));
    assert!(preview.lines[2].source.is_none());
    assert!(preview.lines[0].source.is_some());
    assert!(!preview.lines[0].hyperlinks.is_empty());
}

#[test]
fn zero_limit_preserves_bounded_storage_omissions() {
    let mut output = LiveCommandOutput::default();
    for n in 0..100_000 {
        output.push_str(&format!("retained source line {n}\n"));
    }
    assert!(output.retained_lines() < output.total_lines());
    let retained = output
        .transcript_lines()
        .map(|line| line.into_owned())
        .collect::<Vec<_>>();
    for limit in [0, 1, 2, 30, 50] {
        let preview = output_preview(
            retained
                .iter()
                .map(|line| HyperlinkLine::from(line.clone())),
            /*width*/ 160,
            limit,
            output.total_lines() - output.retained_lines(),
        );
        if limit == 0 {
            assert_eq!(text(&preview.lines), retained.join("\n"));
        } else {
            assert_eq!(preview.lines.len(), limit);
            assert!(text(&preview.lines).contains("lines not retained"));
        }
    }
}
