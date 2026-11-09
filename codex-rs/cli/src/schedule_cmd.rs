//! Thin headless clients for timed ordinary input and its receiving turn.

use crate::InteractiveRemoteOptions;
use crate::SessionArchiveConfigOverrides;
use crate::finalize_session_archive_interactive;
use crate::resolve_remote_endpoint;
use clap::Parser;
use clap::Subcommand;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_app_server_protocol::TurnStatus;
use codex_arg0::Arg0DispatchPaths;
use codex_tui::Cli as TuiCli;
use codex_tui::SessionArchiveCommandOptions;
use codex_tui::SessionScheduleClient;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub(crate) struct ScheduleCommand {
    #[command(subcommand)]
    action: Option<ScheduleCommandAction>,
    /// Existing session UUID or exact name.
    #[arg(long)]
    thread: Option<String>,
    /// ISO time with offset, local HH:MM, or a delay such as 'in 2h'.
    #[arg(long)]
    at: Option<String>,
    /// Ordinary text prompt.
    #[arg(long)]
    message: Option<String>,
    /// Observe the receiving turn. Disconnecting does not cancel pending input.
    #[arg(long, global = true)]
    wait: bool,
    /// Print the pending entry or receiving-turn result as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[arg(
        long = "output-last-message",
        short = 'o',
        global = true,
        value_name = "FILE"
    )]
    last_message_file: Option<PathBuf>,
    #[clap(flatten)]
    remote: InteractiveRemoteOptions,
    #[clap(flatten)]
    pub(crate) config_overrides: SessionArchiveConfigOverrides,
}

#[derive(Debug, Subcommand)]
enum ScheduleCommandAction {
    /// List pending scheduled input.
    List {
        #[arg(long)]
        thread: String,
    },
    /// Remove pending input. Cancellation after due dispatch begins is best effort.
    Cancel { id: String },
    /// Read a scheduled input's pending entry or its receiving turn.
    Result { id: String },
}

pub(crate) async fn run_schedule_command(
    command: ScheduleCommand,
    interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    root_remote: Option<String>,
    root_remote_auth_token_env: Option<String>,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let ScheduleCommand {
        action,
        thread,
        at,
        message,
        wait,
        json,
        last_message_file,
        remote,
        config_overrides,
    } = command;
    if action.is_some() && (thread.is_some() || at.is_some() || message.is_some()) {
        anyhow::bail!("creation flags cannot be combined with list, cancel or result");
    }
    if matches!(
        action,
        Some(ScheduleCommandAction::List { .. } | ScheduleCommandAction::Cancel { .. })
    ) && (wait || last_message_file.is_some())
    {
        anyhow::bail!("--wait and -o apply to creation or result");
    }
    if action.is_none() && last_message_file.is_some() && !wait {
        anyhow::bail!("use --wait with -o when creating scheduled input");
    }
    let creation = action
        .is_none()
        .then(|| -> anyhow::Result<_> {
            let target = thread.ok_or_else(|| anyhow::anyhow!("--thread is required"))?;
            let time = at.ok_or_else(|| anyhow::anyhow!("--at is required"))?;
            let message = message
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("--message must contain text"))?;
            let due = codex_tui::resolve_schedule_time(&time).map_err(anyhow::Error::msg)?;
            Ok((target, due, message))
        })
        .transpose()?;
    let mut cli =
        finalize_session_archive_interactive(interactive, root_config_overrides, config_overrides);
    validate_shared_input_options(&cli)?;
    let remote_addr = remote.remote.or(root_remote);
    cli.remote_addr = remote_addr.clone();
    let endpoint = resolve_remote_endpoint(
        remote_addr,
        remote.remote_auth_token_env.or(root_remote_auth_token_env),
    )?;
    let mut client = SessionScheduleClient::connect(SessionArchiveCommandOptions {
        cli,
        arg0_paths,
        explicit_remote_endpoint: endpoint,
    })
    .await
    .map_err(report)?;
    match action {
        Some(ScheduleCommandAction::List { thread }) => {
            let thread_id = client.resolve_thread(&thread).await.map_err(report)?;
            let items = client.list(thread_id).await.map_err(report)?;
            if json {
                println!("{}", serde_json::to_string(&items)?);
            } else if items.is_empty() {
                println!("No scheduled input is pending.");
            } else {
                for item in items {
                    println!(
                        "{} · fallback auth: {}",
                        codex_tui::schedule_summary(&item),
                        item.fallback_profile.as_deref().unwrap_or("server profile")
                    );
                }
            }
        }
        Some(ScheduleCommandAction::Cancel { id }) => {
            let message = client.cancel(&id).await.map_err(report)?;
            if json {
                println!("{}", serde_json::json!({ "id": id, "message": message }));
            } else {
                println!("{message}");
            }
        }
        Some(ScheduleCommandAction::Result { id }) => {
            let result = if wait {
                observe(&mut client, &id, Observation::Only).await?
            } else {
                client.result(&id).await.map_err(report)?
            };
            print_result(&result, json, last_message_file.as_deref())?;
        }
        None => {
            let Some((target, due, message)) = creation else {
                anyhow::bail!("missing schedule creation arguments");
            };
            let thread_id = client.resolve_thread(&target).await.map_err(report)?;
            let item = client
                .create(thread_id, message, due)
                .await
                .map_err(report)?;
            if json {
                println!("{}", serde_json::to_string(&item)?);
            } else {
                println!(
                    "Scheduled input {} for {}. Fallback auth: {}.",
                    item.id,
                    codex_tui::format_schedule_time(due),
                    item.fallback_profile.as_deref().unwrap_or("server profile")
                );
            }
            if wait {
                let result = observe(&mut client, &item.id, Observation::Starter).await?;
                print_result(&result, json, last_message_file.as_deref())?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_shared_input_options(cli: &TuiCli) -> anyhow::Result<()> {
    if !cli.images.is_empty()
        || cli.model.is_some()
        || cli.oss
        || cli.oss_provider.is_some()
        || cli.sandbox_mode.is_some()
        || cli.auto_review
        || cli.dangerously_bypass_approvals_and_sandbox
        || cli.bypass_hook_trust
        || cli.cwd.is_some()
        || cli.worktree
        || !cli.add_dir.is_empty()
        || cli.approval_policy.is_some()
        || cli.web_search
        || !cli.config_overrides.raw_overrides.is_empty()
    {
        anyhow::bail!(
            "shared input uses the receiving thread's saved configuration; image, model, permission, directory, search and -c overrides are not supported"
        );
    }
    Ok(())
}

pub(crate) enum Observation {
    Only,
    Starter,
}

pub(crate) async fn observe(
    client: &mut SessionScheduleClient,
    id: &str,
    observation: Observation,
) -> anyhow::Result<ThreadQueueResultResponse> {
    let result = tokio::select! {
        result = client.wait(id) => Some(result.map_err(report)?),
        result = tokio::signal::ctrl_c() => {
            result?;
            None
        }
    };
    if result.is_none() {
        match observation {
            Observation::Only => {
                eprintln!("Stopped observing input {id}; pending or admitted input is unchanged.")
            }
            Observation::Starter => match client.interrupt_started(id).await {
                Ok(message) => eprintln!("{message}"),
                Err(error) => {
                    eprintln!("{error:#}. Stopped observing input {id}; it may still execute.")
                }
            },
        }
    }
    result.ok_or_else(|| {
        anyhow::anyhow!(
            "observation interrupted; retrieve input {id} with `codex schedule result {id} --wait`"
        )
    })
}

fn print_result(
    result: &ThreadQueueResultResponse,
    json: bool,
    output: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(result)?);
    } else if let Some(item) = &result.queued_submission {
        println!(
            "Pending input {} · {}{}",
            item.id,
            item.scheduled_at
                .map(codex_tui::format_schedule_time)
                .unwrap_or_default(),
            item.dispatch_error
                .as_ref()
                .map(|error| format!(" · {error}"))
                .unwrap_or_default()
        );
    } else if let Some(text) = codex_tui::scheduled_result_text(result) {
        println!("{text}");
    } else {
        println!(
            "Input admitted to turn {}; result {}.",
            result.turn_id.as_deref().unwrap_or("unknown"),
            result
                .turn
                .as_ref()
                .map(|turn| format!("{:?}", turn.status))
                .unwrap_or_else(|| "unavailable or not yet published".into())
        );
    }
    if let Some(output) = output {
        let turn = result
            .turn
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("result is not available yet"))?;
        if turn.status == TurnStatus::InProgress {
            anyhow::bail!("turn is still running; use --wait with -o");
        }
        std::fs::write(
            output,
            codex_tui::scheduled_result_text(result).unwrap_or_default(),
        )?;
    }
    if let Some(turn) = &result.turn
        && matches!(turn.status, TurnStatus::Failed | TurnStatus::Interrupted)
    {
        anyhow::bail!(
            "receiving turn {} ended as {:?}: {}",
            turn.id,
            turn.status,
            turn.error
                .as_ref()
                .map(|error| error.message.as_str())
                .unwrap_or("no further detail")
        );
    }
    Ok(())
}

pub(crate) fn report(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{error:#}")
}

#[cfg(test)]
#[path = "schedule_cmd_tests.rs"]
mod tests;
