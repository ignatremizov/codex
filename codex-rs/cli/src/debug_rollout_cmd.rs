use std::path::Path;
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
pub(crate) struct DebugRolloutCommand {
    #[command(subcommand)]
    pub(crate) subcommand: DebugRolloutSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum DebugRolloutSubcommand {
    /// Remove inline media from compacted replacement histories in a closed Legacy rollout.
    ///
    /// Requires a rollout belonging to the selected Codex home. Active writers and competing
    /// maintenance jobs are rejected. Paginated and lineage-bearing rollouts are rejected to
    /// preserve immutable fork offsets. Standalone Legacy rollouts without a media-policy marker
    /// are repaired directly.
    Vacuum(DebugRolloutVacuumCommand),
}

#[derive(Debug, Parser)]
pub(crate) struct DebugRolloutVacuumCommand {
    /// Logical .jsonl or physical .jsonl.zst rollout path, absolute or relative to the current cwd.
    #[arg(value_name = "ROLLOUT", value_hint = clap::ValueHint::FilePath)]
    pub(crate) rollout_path: PathBuf,
}

pub(crate) async fn run(command: DebugRolloutCommand, codex_home: &Path) -> anyhow::Result<()> {
    match command.subcommand {
        DebugRolloutSubcommand::Vacuum(command) => {
            let report = codex_core::vacuum_rollout_compacted_media(
                codex_home,
                command.rollout_path.as_path(),
            )
            .await?;
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
