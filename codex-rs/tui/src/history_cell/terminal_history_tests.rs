use super::*;
use pretty_assertions::assert_eq;

#[test]
fn terminal_bookkeeping_is_quiet_but_distinct_in_detailed_and_raw_history() {
    let cells = [
        new_unified_exec_interaction(Some("cat".to_string()), "hello".to_string()),
        new_unified_exec_interaction(Some("cat".to_string()), String::new()),
        new_unified_exec_output_check(Some("cat".to_string())),
    ];
    let mut rendered = Vec::new();
    for cell in cells {
        for width in [0, 1, 12, 80] {
            assert_eq!(cell.display_lines(width), Vec::<Line<'static>>::new());
            let detailed = cell.transcript_lines(width);
            assert_eq!(
                detailed,
                visible_lines(cell.transcript_hyperlink_lines(width))
            );
            if width >= 12 {
                assert!(
                    detailed
                        .iter()
                        .all(|line| line.width() <= usize::from(width))
                );
            }
        }
        assert_eq!(
            cell.transcript_lines(/*width*/ 0),
            Vec::<Line<'static>>::new()
        );
        rendered.push(
            cell.transcript_lines(/*width*/ 80)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        rendered.push(
            cell.raw_lines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    insta::assert_snapshot!(rendered.join("\n---\n"), @"
    ↳ Interacted with background terminal · cat
      └ hello
    ---
    Interacted with background terminal: cat
    hello
    ---
    • Waited for background terminal · cat
    ---
    Waited for background terminal: cat
    ---
    • Checked background terminal output · cat
    ---
    Checked background terminal output: cat
    ");
}
