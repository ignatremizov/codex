use super::CommandLayout;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCall;
use crate::exec_cell::ExecCell;
use crate::history_cell::HistoryCell;
use crate::render::highlight::MAX_HIGHLIGHT_LINE_BYTES;
use crate::render::highlight::highlight_code_to_lines;
use codex_app_server_protocol::CommandExecutionSource;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::text::Line;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn shell_statements_keep_literals_substitutions_and_loop_headers() {
    let script = "set -e; printf '%s;' \"界;\"; for ((i=0; i<2; i++)); do echo \"$i\"; done; case \"$x\" in a) echo one;; *) echo two;; esac; echo \"$(echo nested; echo substitution)\"";
    let rows = CommandLayout::new(script).into_lines();
    let rendered = rows
        .iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(rendered);
    assert!(rows.iter().all(|row| {
        row.source
            .as_ref()
            .is_some_and(|source| source.text.as_ref() == script)
    }));
    assert!(rows.windows(2).all(|pair| {
        Arc::ptr_eq(
            &pair[0].source.as_ref().expect("source").text,
            &pair[1].source.as_ref().expect("source").text,
        )
    }));
}

#[test]
fn node_heredoc_uses_javascript_without_splitting_literals_or_for_headers() {
    let code = "const text = \"a;b\"; const pattern = /[;()]/; for (let i=0;i<2;i++) { console.log(`x;${i}`); } // comment; stays\n";
    let script = format!("node <<'NODE'\n{code}NODE\nprintf done");
    let layout = CommandLayout::new(&script);
    assert_eq!(
        layout.lines[1..2],
        highlight_code_to_lines(code, "javascript"),
    );
    assert_snapshot!(
        layout
            .into_lines()
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn opaque_heredocs_and_node_file_input_preserve_source_rows() {
    for script in [
        "cat <<'TEXT'\nfirst; second\nTEXT\n",
        "node file.js <<'TEXT'\nfirst; second\nTEXT\n",
        "node 3<<'TEXT'\nfirst; second\nTEXT\n",
        "node <<'TEXT' file.js\nfirst; second\nTEXT\n",
        "node <<'TEXT' -e 'console.log(1)'\nfirst; second\nTEXT\n",
        "node <<'TEXT' <input.js\nfirst; second\nTEXT\n",
        "node <<TEXT\nfirst; second\nTEXT\n",
        "node <<'TEXT'\nTEXT\n",
        "printf '%s' 'unclosed; quote",
    ] {
        assert_eq!(
            CommandLayout::new(script)
                .into_lines()
                .iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>(),
            script.lines().map(str::to_owned).collect::<Vec<_>>(),
            "{script}",
        );
    }
}

#[test]
fn indented_node_heredoc_retains_its_closing_marker_and_every_source_row() {
    let script = "node <<-'JS'\n\tconst a=1; const b=2;\n\tJS\nprintf done";
    let layout = CommandLayout::new(script);
    assert_eq!(
        layout.lines.iter().map(Line::to_string).collect::<Vec<_>>(),
        script.lines().map(str::to_owned).collect::<Vec<_>>(),
    );
    assert_snapshot!(
        layout
            .into_lines()
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn javascript_template_interpolations_remain_one_literal_display_fragment() {
    let template = "const t = `${function () { foo(); bar(); }}`;";
    let script = format!("node <<'JS'\n{template} done();\nJS");
    assert_eq!(
        CommandLayout::new(&script)
            .into_lines()
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>(),
        vec!["node <<'JS'", template, "done();", "JS"],
    );
}

#[test]
fn oversized_command_keeps_the_bounded_highlighter_fallback() {
    let script = format!("{}; echo done", "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1));
    let rows = CommandLayout::new(&script).into_lines();
    assert_eq!(
        rows.iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>(),
        vec![script],
    );
}

#[test]
fn completed_command_previews_and_details_expand_but_raw_history_keeps_source() {
    let script = "printf '%s;' '界';  echo done";
    let command = vec!["bash".to_owned(), "-lc".to_owned(), script.to_owned()];
    let cell = ExecCell::new(
        ExecCall {
            call_id: "statements".to_owned(),
            command: command.clone(),
            parsed: Vec::new(),
            output: Some(CommandOutput::new(
                /*exit_code*/ 0,
                "界;done\n".to_owned(),
            )),
            source: CommandExecutionSource::UnifiedExecStartup,
            user_shell_response_handling: None,
            start_time: None,
            duration: Some(Duration::from_millis(/*millis*/ 5)),
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    );
    let preview = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let details = cell
        .transcript_lines(/*width*/ 80)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(format!("Preview\n{preview}\n\nDetails\n{details}"));
    assert_eq!(cell.raw_lines()[0].to_string(), format!("$ {script}"));
    assert_eq!(cell.iter_calls().next().expect("call").command, command);
}
