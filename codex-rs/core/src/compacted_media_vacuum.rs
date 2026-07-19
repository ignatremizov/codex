use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use codex_history::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::CompactedMediaVacuumReport;

use crate::context::CompactedImageOmission;
use crate::context::ContextualUserFragment;

/// Physically removes inline media from compacted rollout replacement histories.
///
/// Requires a canonical rollout belonging to the selected Codex home's sessions or archive.
/// Maintenance and writer ownership are acquired inside the blocking worker and retained through
/// replacement, even if the caller cancels its wait. An open thread or competing maintenance job
/// returns `WouldBlock`; an unprovable home or thread identity is rejected before any rewrite.
/// Only standalone Legacy histories are eligible: physical rewrites would invalidate the byte
/// offsets persisted by Paginated descendants, including descendants created after this check.
pub async fn vacuum_rollout_compacted_media(
    codex_home: &Path,
    rollout_path: &Path,
) -> io::Result<CompactedMediaVacuumReport> {
    let codex_home = codex_home.to_path_buf();
    let rollout_path = rollout_path.to_path_buf();
    let policy = codex_rollout::CompactedMediaVacuumPolicy {
        reopenable_image_omission: CompactedImageOmission::reopenable_local_image().render(),
        unavailable_image_omission: CompactedImageOmission::unavailable().render(),
        mixed_image_omission: CompactedImageOmission::mixed().render(),
    };
    tokio::task::spawn_blocking(move || {
        let home = fs::canonicalize(codex_home)?;
        let _maintenance = codex_rollout::try_acquire_rollout_maintenance_lock(&home)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "rollout maintenance is already running for the selected Codex home",
                )
            })?;
        // Resolve the parent independently: a logical .jsonl path may exist only as .jsonl.zst.
        let requested = codex_rollout::plain_rollout_path(&rollout_path);
        let parent = requested
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent)?;
        let relative_parent = parent.strip_prefix(&home).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout is outside the selected Codex home",
            )
        })?;
        if !relative_parent.starts_with(codex_rollout::SESSIONS_SUBDIR)
            && !relative_parent.starts_with(codex_rollout::ARCHIVED_SESSIONS_SUBDIR)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout must belong to the selected Codex home's sessions or archived_sessions",
            ));
        }
        let filename = requested.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "rollout path has no filename")
        })?;
        let path = parent.join(filename);
        let thread_id = codex_rollout::thread_id_from_rollout_path(&path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout filename does not identify a canonical thread",
            )
        })?;
        let coordinator = Arc::new(codex_rollout::WriterLockCoordinator::new(&home));
        let _writer = coordinator.acquire(thread_id)?;
        // Neither sibling may redirect reads or replacement to an unrelated rollout.
        for sibling in [&path, &path.with_extension("jsonl.zst")] {
            match fs::symlink_metadata(sibling) {
                Ok(metadata) if !metadata.file_type().is_file() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "rollout siblings must be regular files, not symlinks or directories",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        // Metadata is always the first rollout record. Bound the identity probe, including
        // decompression, before the low-level streaming vacuum inspects the complete file.
        const MAX_IDENTITY_BYTES: usize = 1024 * 1024;
        let prefix = codex_rollout::read_rollout_prefix(&path, MAX_IDENTITY_BYTES)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rollout is not a file"))?;
        let first = prefix
            .split_inclusive(|byte| *byte == b'\n')
            .find(|line| !line.iter().all(u8::is_ascii_whitespace))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "rollout has no session metadata")
            })?;
        if prefix.len() == MAX_IDENTITY_BYTES && !first.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rollout session metadata exceeds the bounded identity probe",
            ));
        }
        let record = codex_rollout::parse_rollout_line_bytes(first).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot establish rollout identity from its bounded metadata: {error}"),
            )
        })?;
        let RolloutItem::SessionMeta(meta) = record.item else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rollout does not begin with session metadata",
            ));
        };
        if meta.meta.id != thread_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rollout session metadata does not match the filename's thread identity",
            ));
        }
        // Reference sources must be Paginated. Maintenance ownership excludes migration and
        // writer ownership excludes mutation while we establish that this is standalone Legacy.
        // An empty reference-index lookup would not fence concurrent reference creation.
        if meta.meta.history_mode != ThreadHistoryMode::Legacy
            || meta.meta.history_base.is_some()
            || meta.meta.subagent_history_start_ordinal.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "physical media vacuum requires standalone Legacy history; Paginated or lineage-bearing rollouts may have immutable byte-offset references",
            ));
        }
        codex_rollout::vacuum_compacted_media(&path, &policy)
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
#[path = "compacted_media_vacuum_tests.rs"]
mod tests;
