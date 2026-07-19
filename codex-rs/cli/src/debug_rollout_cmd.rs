use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
pub(crate) struct DebugRolloutCommand {
    #[command(subcommand)]
    pub(crate) subcommand: DebugRolloutSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum DebugRolloutSubcommand {
    /// Remove inline media from compacted replacement histories in a closed rollout.
    ///
    /// Legacy rollouts without a media-policy marker are repaired directly. Do not run this
    /// command while any Codex process has the session open for mutation; use `codex fork` when
    /// two live branches are needed.
    Vacuum(DebugRolloutVacuumCommand),
}

#[derive(Debug, Parser)]
pub(crate) struct DebugRolloutVacuumCommand {
    /// Rollout JSONL file to vacuum.
    #[arg(value_name = "ROLLOUT", value_hint = clap::ValueHint::FilePath)]
    pub(crate) rollout_path: PathBuf,
}

pub(crate) async fn run(command: DebugRolloutCommand) -> anyhow::Result<()> {
    match command.subcommand {
        DebugRolloutSubcommand::Vacuum(command) => {
            let report =
                codex_core::vacuum_rollout_compacted_media(command.rollout_path.as_path()).await?;
            println!(
                "Vacuumed {}: {} -> {} bytes; rewrote {} compacted records; removed {} inline images ({} bytes).",
                command.rollout_path.display(),
                report.bytes_before,
                report.bytes_after,
                report.records_rewritten,
                report.omitted_image_count,
                report.omitted_inline_media_bytes,
            );
        }
    }
    Ok(())
}
