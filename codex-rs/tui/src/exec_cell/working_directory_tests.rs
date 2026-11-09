use super::*;
use crate::exec_cell::ActiveExecCall;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::model::ExecCall;
use crate::history_cell::HistoryCell;
use crate::terminal_hyperlinks::lines_with_sources_eq;
use codex_app_server_protocol::CommandExecutionSource;
use codex_protocol::parse_command::ParsedCommand;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn directory(cwd: &str, previous: &str) -> CommandDirectory {
    let base = LegacyAppPathString::from_string("/workspace")
        .to_inferred_path_uri()
        .unwrap();
    CommandDirectory::new(
        LegacyAppPathString::from_string(cwd),
        base,
        Some(&LegacyAppPathString::from_string(previous)),
    )
}

fn command(directory: CommandDirectory) -> ExecCell {
    ExecCell::new(
        ExecCall {
            call_id: "command".into(),
            command: vec!["printf".into(), "done".into()],
            parsed: Vec::new(),
            output: Some(CommandOutput::new(/*exit_code*/ 0, "done\n".into())),
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            start_time: None,
            duration: Some(Duration::from_millis(/*millis*/ 5)),
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    )
    .with_command_directory(Some(directory))
}

fn text(lines: &[HyperlinkLine]) -> String {
    lines
        .iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn directory_changes_establish_context_until_the_next_change() {
    let cases = [
        ("/workspace/service-a", "/workspace"),
        ("/workspace/service-a", "/workspace/service-a"),
        ("/workspace/service-b", "/workspace/service-a"),
        ("/workspace", "/workspace/service-b"),
    ];
    let rendered = cases.map(|(cwd, previous)| {
        text(&command(directory(cwd, previous)).display_hyperlink_lines(/*width*/ 80))
    });
    assert_snapshot!(rendered.join("\n\n"), @"
      ↳ cwd service-a
    • Ran printf done
      └ done

    • Ran printf done
      └ done

      ↳ cwd service-b
    • Ran printf done
      └ done

      ↳ cwd .
    • Ran printf done
      └ done
    ");
}

#[test]
fn full_paths_keep_shell_source_and_raw_history_unchanged() {
    let cell = command(directory("/workspace/service-a", "/workspace/service-a"));
    let short = cell.transcript_hyperlink_lines(/*width*/ 80);
    let full = cell.command_directory().unwrap().replace_lines(
        /*width*/ 80,
        CommandDirectoryDisplay::Full,
        short.clone(),
    );
    assert_snapshot!(text(&full), @"
      ↳ cwd /workspace/service-a
    $ printf done
    done
    ✓ • 5ms
    ");
    assert!(lines_with_sources_eq(&full[1..], &short));
    assert_eq!(
        cell.raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["$ printf done", "done", "✓ • 5ms"],
    );
    let narrow = cell.command_directory().unwrap().replace_lines(
        /*width*/ 12,
        CommandDirectoryDisplay::Full,
        cell.transcript_hyperlink_lines(/*width*/ 12),
    );
    let original = cell.transcript_hyperlink_lines(/*width*/ 12);
    let source = |lines: &[HyperlinkLine]| {
        lines
            .iter()
            .filter(|line| line.source.is_some())
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(lines_with_sources_eq(&source(&narrow), &source(&original)));
    assert!(narrow.iter().all(|line| line.line.width() <= 12));
}

#[test]
fn exploration_does_not_merge_commands_from_different_directories() {
    let exploration = |cwd| {
        let mut cell = command(directory(cwd, "/workspace"));
        cell.group.calls[0].command = vec!["ls".into()];
        cell.group.calls[0].parsed = vec![ParsedCommand::ListFiles {
            cmd: "ls".into(),
            path: None,
        }];
        cell
    };
    let mut group = exploration("/workspace/service-a");
    assert!(
        group
            .append_completed(exploration("/workspace/service-a"))
            .is_ok()
    );
    assert!(
        group
            .append_completed(exploration("/workspace/service-b"))
            .is_err()
    );
    let other = directory("/workspace/service-b", "/workspace/service-a");
    assert!(!group.add_call(
        ActiveExecCall {
            call_id: "other".into(),
            command: vec!["ls".into()],
            parsed: vec![ParsedCommand::ListFiles {
                cmd: "ls".into(),
                path: None
            }],
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            interaction_input: None,
        },
        Some(&other),
    ));
    assert_eq!(group.iter_calls().count(), 2);
}

#[test]
fn foreign_paths_use_their_own_convention_and_escape_controls() {
    let base = LegacyAppPathString::from_string(r"C:\work")
        .to_inferred_path_uri()
        .unwrap();
    let windows_directory = CommandDirectory::new(
        LegacyAppPathString::from_string(r"C:\work\service"),
        base,
        Some(&LegacyAppPathString::from_string("c:/work/service")),
    );
    assert!(
        windows_directory
            .lines(/*width*/ 80, CommandDirectoryDisplay::Changes)
            .is_empty()
    );
    assert_eq!(
        text(&windows_directory.lines(/*width*/ 80, CommandDirectoryDisplay::Relative)),
        "  ↳ cwd service",
    );
    let directory = directory("/outside/a\nb\tc\u{1b}[31m", "/workspace");
    assert_eq!(
        text(&directory.lines(/*width*/ 80, CommandDirectoryDisplay::Full)),
        r"  ↳ cwd /outside/a\nb\tc",
    );
}
