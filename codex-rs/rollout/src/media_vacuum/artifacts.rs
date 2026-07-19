//! Explicit lifecycle cleanup of media-vacuum artifacts under writer ownership.

use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io;
use std::path::Path;
use uuid::Uuid;

/// Removes recoverable backups and interrupted temporary files associated with `path`.
///
/// Thread deletion and archival call this after validating that `path` belongs to the selected
/// thread so a retained backup cannot later resurrect a deleted or moved rollout.
pub fn remove_compacted_media_vacuum_backups(path: &Path) -> io::Result<()> {
    let path = crate::plain_rollout_path(path);
    let path = path.as_path();
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
    let temporary_prefix = format!(".{file_name}.media-vacuum-");
    // Remove authority before its payloads, including when cleanup is interrupted midway.
    match fs::remove_file(super::recovery::manifest_path(path)?) {
        Ok(()) => sync_parent_directory(parent)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut removed = false;
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_temporary = name
            .strip_prefix(temporary_prefix.as_str())
            .and_then(|name| name.strip_suffix(".tmp"))
            .is_some_and(|temporary_id| !temporary_id.is_empty());
        if compacted_media_backup_id(name, file_name).is_some() || is_temporary {
            fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        sync_parent_directory(parent)?;
    }
    Ok(())
}

/// Removes a compressed sibling hidden by an existing canonical plain rollout.
pub fn remove_obsolete_compressed_rollout_sibling(path: &Path) -> io::Result<()> {
    let plain_path = crate::compression::plain_rollout_path(path);
    if !plain_path.exists() {
        return Ok(());
    }
    let compressed_path = crate::compression::compressed_rollout_path(plain_path.as_path());
    match fs::remove_file(compressed_path.as_path()) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    let parent = plain_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_parent_directory(parent)
}

pub(super) fn compacted_media_backup_id(name: &str, file_name: &str) -> Option<Uuid> {
    let (backup_file_name, backup_id) = compacted_media_backup_parts(name)?;
    (backup_file_name == file_name).then_some(backup_id)
}

/// Returns the canonical rollout file name encoded by a media-vacuum backup name.
pub(crate) fn compacted_media_backup_rollout_file_name(name: &str) -> Option<&str> {
    compacted_media_backup_parts(name).map(|(file_name, _)| file_name)
}

/// Returns the canonical rollout file name encoded by any media-vacuum artifact name.
pub fn compacted_media_vacuum_artifact_rollout_file_name(name: &str) -> Option<&str> {
    compacted_media_backup_rollout_file_name(name)
        .or_else(|| name.strip_prefix('.')?.strip_suffix(".media-vacuum.json"))
        .or_else(|| {
            let name = name.strip_prefix('.')?;
            let (file_name, temporary_id) = name.rsplit_once(".media-vacuum-")?;
            let temporary_id = temporary_id.strip_suffix(".tmp")?;
            (!file_name.is_empty() && !temporary_id.is_empty()).then_some(file_name)
        })
}

fn compacted_media_backup_parts(name: &str) -> Option<(&str, Uuid)> {
    let name = name.strip_prefix('.')?;
    let (file_name, backup_id) = name.rsplit_once(".pre-media-vacuum-")?;
    let backup_id = backup_id.strip_suffix(".bak")?;
    if file_name.is_empty() {
        return None;
    }
    Some((file_name, Uuid::parse_str(backup_id).ok()?))
}

#[cfg(unix)]
pub(super) fn cleanup_completed_backup(parent: &Path, backup_path: &Path) -> io::Result<()> {
    fs::remove_file(backup_path)?;
    sync_parent_directory(parent)
}

#[cfg(not(unix))]
pub(super) fn cleanup_completed_backup(_parent: &Path, _backup_path: &Path) -> io::Result<()> {
    // Keep the recovery link when the platform cannot provide a directory durability barrier.
    // The next manual vacuum, archive, or delete operation removes retained backups explicitly.
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}
