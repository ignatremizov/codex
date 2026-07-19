use std::io;
use std::path::Path;

use codex_rollout::CompactedMediaVacuumReport;

use crate::context::CompactedImageOmission;
use crate::context::ContextualUserFragment;

/// Physically removes inline media from superseded compacted rollout checkpoints.
///
/// The rollout must not be open for mutation in another Codex process. Resume the session once
/// before invoking this function so reconstruction can persist the protected repaired checkpoint
/// that authorizes removal from superseded snapshots. Mutating one rollout from multiple resumed
/// Codex processes is operator error; use `codex fork` for concurrent branches.
pub async fn vacuum_rollout_compacted_media(
    rollout_path: &Path,
) -> io::Result<CompactedMediaVacuumReport> {
    let rollout_path = rollout_path.to_path_buf();
    let policy = codex_rollout::CompactedMediaVacuumPolicy {
        reopenable_image_omission: CompactedImageOmission::reopenable_local_image().render(),
        unavailable_image_omission: CompactedImageOmission::unavailable().render(),
    };
    tokio::task::spawn_blocking(move || {
        codex_rollout::vacuum_compacted_media(rollout_path.as_path(), &policy)
    })
    .await
    .map_err(io::Error::other)?
}
