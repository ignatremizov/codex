use super::ScheduleCommandAction;
use super::validate_shared_input_options;
use crate::MultitoolCli;
use crate::Subcommand;
use clap::Parser;
use pretty_assertions::assert_eq;

#[test]
fn schedule_creation_and_result_options_parse_on_their_own_commands() -> anyhow::Result<()> {
    let cli = MultitoolCli::try_parse_from([
        "codex",
        "schedule",
        "--thread",
        "saved-task",
        "--at",
        "in 2h",
        "--message",
        "review results",
    ])?;
    let Some(Subcommand::Schedule(command)) = cli.subcommand else {
        anyhow::bail!("missing schedule command");
    };
    assert_eq!(
        (command.thread, command.at, command.message),
        (
            Some("saved-task".into()),
            Some("in 2h".into()),
            Some("review results".into())
        ),
    );
    let cli = MultitoolCli::try_parse_from([
        "codex",
        "schedule",
        "result",
        "input-id",
        "--json",
        "--wait",
        "-o",
        "answer.txt",
    ])?;
    let Some(Subcommand::Schedule(command)) = cli.subcommand else {
        anyhow::bail!("missing result command");
    };
    assert!(
        matches!(command.action, Some(ScheduleCommandAction::Result { id }) if id == "input-id")
    );
    assert_eq!(
        (command.wait, command.json, command.last_message_file),
        (true, true, Some("answer.txt".into()))
    );
    Ok(())
}

#[test]
fn shared_input_rejects_runtime_overrides_before_accepting_input() -> anyhow::Result<()> {
    for args in [
        vec!["codex", "--model", "custom"],
        vec!["codex", "--sandbox", "read-only"],
        vec!["codex", "--cd", "/workspace"],
        vec!["codex", "--search"],
        vec!["codex", "-c", "model=\"custom\""],
    ] {
        let cli = MultitoolCli::try_parse_from(args)?;
        let mut interactive = cli.interactive;
        interactive.config_overrides = cli.config_overrides;
        assert!(validate_shared_input_options(&interactive).is_err());
    }
    assert!(
        validate_shared_input_options(&MultitoolCli::try_parse_from(["codex"])?.interactive)
            .is_ok()
    );
    Ok(())
}
