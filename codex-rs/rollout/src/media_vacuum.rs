//! Offline compacted-media rewriting with exact-byte preservation and journaled recovery.

use std::fs;
use std::fs::File;
use std::fs::FileTimes;
use std::io;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use tracing::warn;

mod artifacts;
mod json_spans;
mod preflight;
mod record_validation;
mod recovery;
mod rewrite;

pub(crate) use artifacts::compacted_media_backup_rollout_file_name;
pub use artifacts::compacted_media_vacuum_artifact_rollout_file_name;
pub use artifacts::remove_compacted_media_vacuum_backups;
pub use artifacts::remove_obsolete_compressed_rollout_sibling;
pub(crate) use artifacts::sync_parent_directory;
pub(crate) use recovery::open_recovery_source;
pub(crate) use recovery::recover_compacted_media_backup_if_needed;
pub(crate) use recovery::retire_recovery_before_append;

use preflight::parse_rollout_record;
use preflight::preflight_compressed_rollout;
use preflight::preflight_rollout;
use preflight::read_bounded_rollout_record;
use rewrite::compacted_media_replacements;
use rewrite::write_replacements;

#[cfg(test)]
use artifacts::compacted_media_backup_id;
#[cfg(test)]
use uuid::Uuid;

/// Bounded replacement text used while vacuuming historic checkpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedMediaVacuumPolicy {
    /// Replacement for an image that retains a canonical local source-path wrapper.
    pub reopenable_image_omission: String,
    /// Replacement for an image without a durable source reference.
    pub unavailable_image_omission: String,
    /// Replacement when a checkpoint contains both reopenable and unavailable images.
    pub mixed_image_omission: String,
}

/// Storage reduction and media-omission totals from a completed rollout vacuum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactedMediaVacuumReport {
    /// Physical rollout size before replacement.
    pub bytes_before: u64,
    /// Physical rollout size after replacement.
    pub bytes_after: u64,
    /// Number of compacted JSONL records rewritten.
    pub records_rewritten: usize,
    /// Number of inline image items replaced.
    pub omitted_image_count: usize,
    /// Serialized image URL bytes replaced.
    pub omitted_inline_media_bytes: u64,
}

/// Rewrites a closed rollout to remove media from compacted replacement histories.
///
/// The caller must hold maintenance and writer authority and ensure no immutable history range
/// refers to bytes being rewritten, including references created concurrently. This low-level
/// function does not establish that authority; the public command permits only standalone Legacy
/// history. Rollouts without a media-policy marker are repaired directly by sanitizing every valid
/// compacted replacement history. Once a rollout contains a marker, every marked record must be a
/// valid sanitized checkpoint so its potentially unsummarized suffix remains protected.
pub fn vacuum_compacted_media(
    path: &Path,
    policy: &CompactedMediaVacuumPolicy,
) -> io::Result<CompactedMediaVacuumReport> {
    let plain_path = crate::compression::plain_rollout_path(path);
    let compressed_path = crate::compression::compressed_rollout_path(plain_path.as_path());
    if !plain_path.exists() && !compressed_path.exists() {
        recover_compacted_media_backup_if_needed(plain_path.as_path())?;
    }
    let physical_path = if plain_path.exists() {
        plain_path.as_path()
    } else {
        compressed_path.as_path()
    };
    let selected_compressed_rollout = physical_path == compressed_path.as_path();
    let physical_metadata = fs::metadata(physical_path)?;
    let physical_bytes_before = physical_metadata.len();
    let physical_modified = physical_metadata.modified().ok();
    let preflight = if physical_path == compressed_path.as_path() {
        preflight_compressed_rollout(physical_path)?
    } else {
        preflight_rollout(physical_path)?
    };
    // A marker means the rollout has an explicit summarized-prefix boundary. Do not reinterpret a
    // malformed or false marker as checkpointless pre-policy data because its suffix may be the only
    // persisted copy of unsummarized media.
    if preflight.found_invalid_media_policy_marker {
        return Err(io::Error::other(
            "refusing compacted-media vacuum with an invalid sanitized replacement-history checkpoint",
        ));
    }
    if plain_path.exists() && compressed_path.exists() {
        if !preflight.found_valid_rollout_record {
            return Err(io::Error::other(
                "refusing to remove a compressed rollout sibling without a valid canonical plain record",
            ));
        }
        remove_obsolete_compressed_rollout_sibling(plain_path.as_path())?;
    }
    if !preflight.found_rewritable_media {
        if preflight.found_valid_rollout_record {
            remove_compacted_media_vacuum_backups(plain_path.as_path())?;
        }
        return Ok(CompactedMediaVacuumReport {
            bytes_before: physical_bytes_before,
            bytes_after: physical_bytes_before,
            ..Default::default()
        });
    }
    let path = crate::compression::materialize_rollout_for_append_blocking(path)?;
    let path = path.as_path();
    let metadata = fs::metadata(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::other(format!(
                "rollout path has no UTF-8 file name: {}",
                path.display()
            ))
        })?;
    remove_compacted_media_vacuum_backups(path)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{file_name}.media-vacuum-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(metadata.permissions())?;

    let mut report = CompactedMediaVacuumReport {
        bytes_before: physical_bytes_before,
        ..Default::default()
    };
    {
        let mut reader = BufReader::new(File::open(path)?);
        let mut writer = BufWriter::new(temporary.as_file_mut());
        let mut line = Vec::new();
        while read_bounded_rollout_record(&mut reader, &mut line, path)? {
            let json = line.strip_suffix(b"\n").unwrap_or(line.as_slice());
            let json = json.strip_suffix(b"\r").unwrap_or(json);
            if json.iter().all(u8::is_ascii_whitespace) {
                writer.write_all(line.as_slice())?;
                line.clear();
                continue;
            }

            let Some(spans) = parse_rollout_record(json) else {
                warn!(
                    path = %path.display(),
                    "preserving rejected rollout record during compacted-media vacuum"
                );
                writer.write_all(line.as_slice())?;
                line.clear();
                continue;
            };
            let replacements = compacted_media_replacements(&spans, json, policy, &mut report)?;
            if replacements.is_empty() {
                writer.write_all(line.as_slice())?;
            } else {
                write_replacements(&mut writer, json, &replacements)?;
                writer.write_all(&line[json.len()..])?;
                report.records_rewritten = report.records_rewritten.saturating_add(1);
            }
            line.clear();
        }
        writer.flush()?;
    }
    if let Some(modified) = physical_modified {
        temporary
            .as_file()
            .set_times(FileTimes::new().set_modified(modified))?;
    }
    temporary.as_file().sync_all()?;
    let rewritten_len = temporary.as_file().metadata()?.len();

    if report.records_rewritten == 0 {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }

    let backup_path = recovery::prepare_backup(path)?;
    match temporary.persist(path) {
        Ok(_) => {}
        Err(err) => {
            if !path.exists() {
                recover_compacted_media_backup_if_needed(path)?;
            }
            return Err(err.error);
        }
    }
    report.bytes_after = rewritten_len;
    if let Err(err) = sync_parent_directory(parent) {
        // Replacement has committed but is not durably published. Keep the recoverable backup and
        // report failure so the offline command cannot claim a crash-safe completion.
        return Err(io::Error::other(format!(
            "failed to sync rollout directory after compacted-media vacuum of {}: {err}",
            path.display()
        )));
    }
    if cfg!(unix) || !selected_compressed_rollout {
        remove_obsolete_compressed_rollout_sibling(path)?;
    }
    if let Err(err) = recovery::finish_backup(path, backup_path.as_path()) {
        warn!(
            %err,
            path = %path.display(),
            "failed to clean up compacted-media vacuum backup"
        );
    }
    Ok(report)
}

#[cfg(test)]
#[path = "media_vacuum_tests.rs"]
mod tests;
