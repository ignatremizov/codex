use std::io;
use std::path::Path;

use codex_rollout::CompactedMediaVacuumReport;

use crate::context::CompactedImageOmission;
use crate::context::ContextualUserFragment;

/// Physically removes inline media from compacted rollout replacement histories.
///
/// The rollout must not be open for mutation in another Codex process. Legacy rollouts without a
/// media-policy marker are repaired directly by atomically sanitizing every valid compacted
/// replacement history. Mutating one rollout from multiple resumed Codex processes is operator
/// error; use `codex fork` for concurrent branches.
pub async fn vacuum_rollout_compacted_media(
    rollout_path: &Path,
) -> io::Result<CompactedMediaVacuumReport> {
    let rollout_path = rollout_path.to_path_buf();
    let policy = codex_rollout::CompactedMediaVacuumPolicy {
        reopenable_image_omission: CompactedImageOmission::reopenable_local_image().render(),
        unavailable_image_omission: CompactedImageOmission::unavailable().render(),
        mixed_image_omission: CompactedImageOmission::mixed().render(),
    };
    tokio::task::spawn_blocking(move || {
        codex_rollout::vacuum_compacted_media(rollout_path.as_path(), &policy)
    })
    .await
    .map_err(io::Error::other)?
}
