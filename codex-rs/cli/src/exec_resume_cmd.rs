//! Prompt-bearing exec resume reuses shared queue admission and canonical results.

use crate::schedule_cmd::Observation;
use crate::schedule_cmd::observe;
use crate::schedule_cmd::report;
use crate::schedule_cmd::validate_shared_input_options;
use codex_arg0::Arg0DispatchPaths;
use codex_exec::Cli as ExecCli;
use codex_exec::Command;
use codex_tui::Cli as TuiCli;
use codex_tui::SessionArchiveCommandOptions;
use codex_tui::SessionScheduleClient;
use std::io::IsTerminal;

pub(crate) fn applies(cli: &ExecCli) -> bool {
    matches!(&cli.command, Some(Command::Resume(args)) if
        args.prompt.is_some() || cli.prompt.is_some() || !std::io::stdin().is_terminal())
}

pub(crate) async fn run(
    exec: ExecCli,
    mut interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let Some(Command::Resume(args)) = &exec.command else {
        anyhow::bail!("shared exec routing requires resume");
    };
    *interactive.shared = (*exec.shared).clone();
    interactive.strict_config = exec.strict_config;
    interactive
        .config_overrides
        .raw_overrides
        .clone_from(&exec.config_overrides.raw_overrides);
    validate_shared_input_options(&interactive)?;
    if !args.images.is_empty()
        || exec.ephemeral
        || exec.ignore_user_config
        || exec.ignore_rules
        || exec.output_schema.is_some()
        || exec.cyber_access_program.is_some()
        || exec.thread_source.is_some()
    {
        anyhow::bail!(
            "shared exec resume supports text input and output options; images, ephemeral, config/rules suppression, output schema and execution overrides must be configured on the receiving thread"
        );
    }
    let prompt = codex_exec::resolve_prompt(args.prompt.clone().or_else(|| exec.prompt.clone()));
    if prompt.trim().is_empty() {
        anyhow::bail!("resume prompt must contain text");
    }
    let cwd = if args.last && !args.all {
        Some(std::env::current_dir()?)
    } else {
        None
    };
    let mut client = SessionScheduleClient::connect(SessionArchiveCommandOptions {
        cli: interactive,
        arg0_paths,
        explicit_remote_endpoint: None,
    })
    .await
    .map_err(report)?;
    let thread_id = if let Some(target) = &args.session_id {
        client.resolve_thread(target).await.map_err(report)?
    } else if args.last {
        client.latest_thread(cwd).await.map_err(report)?
    } else {
        anyhow::bail!("resume requires a session UUID/name or --last");
    };
    let item = client
        .create(thread_id, prompt, chrono::Utc::now().timestamp())
        .await
        .map_err(report)?;
    eprintln!("Accepted input {} for thread {thread_id}.", item.id);
    let result = observe(&mut client, &item.id, Observation::Starter).await?;
    codex_exec::write_shared_turn_result(&result, exec.json, exec.last_message_file.as_deref())
}
