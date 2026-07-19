//! A single durable manifest authorizes recovery of one exact pre-rewrite source.
//!
//! Filename resemblance is not recovery authority. Readers validate and retain an open backup
//! handle without publishing it. Only the caller holding the rollout writer lease may restore it.

use std::fs;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use super::artifacts::cleanup_completed_backup;
use super::artifacts::compacted_media_backup_id;
use super::artifacts::sync_parent_directory;
use super::preflight::parse_rollout_record;
use super::preflight::preflight_rollout_reader;
use super::preflight::read_bounded_rollout_record;
use crate::RolloutItem;
use crate::rollout_file_name::RolloutFileName;

const MAX_MANIFEST_BYTES: u64 = 16 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryManifest {
    version: u32,
    canonical: String,
    backup: String,
    format: BackupFormat,
    source_bytes: u64,
    source_sha256: String,
    thread_id: Option<ThreadId>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackupFormat {
    PlainJsonl,
}

pub(super) fn manifest_path(path: &Path) -> io::Result<PathBuf> {
    let file_name = canonical_file_name(path)?;
    Ok(path.with_file_name(format!(".{file_name}.media-vacuum.json")))
}

fn canonical_file_name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| io::Error::other(format!("invalid rollout filename: {}", path.display())))
}

fn fingerprint(file: &mut File) -> io::Result<(u64, String)> {
    file.rewind()?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("rollout byte count overflow"))?;
    }
    file.rewind()?;
    Ok((bytes, format!("{:x}", digest.finalize())))
}

fn source_thread_id(file: &mut File, path: &Path) -> io::Result<Option<ThreadId>> {
    file.rewind()?;
    let mut reader = BufReader::new(&mut *file);
    let mut line = Vec::new();
    let mut thread_id = None;
    while read_bounded_rollout_record(&mut reader, &mut line, path)? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Avoid materializing image-bearing records just to recognize the metadata header.
        if parse_rollout_record(&line).is_some_and(|record| {
            record
                .object_value("type")
                .and_then(|value| value.as_string(&line))
                .is_some_and(|kind| kind == "session_meta")
        }) {
            let record = crate::parse_rollout_line_bytes(&line).map_err(io::Error::other)?;
            if let RolloutItem::SessionMeta(meta) = record.item {
                thread_id = Some(meta.meta.id);
            }
        }
        break;
    }
    file.rewind()?;
    if let Some(name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(RolloutFileName::parse)
        && thread_id != Some(name.thread_id())
    {
        return Err(io::Error::other(
            "media-vacuum source metadata does not match its thread filename",
        ));
    }
    Ok(thread_id)
}

/// Registers a synced preimage before the canonical file can be replaced.
pub(super) fn prepare_backup(path: &Path) -> io::Result<PathBuf> {
    let file_name = canonical_file_name(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let backup_name = format!(".{file_name}.pre-media-vacuum-{}.bak", Uuid::now_v7());
    let backup_path = path.with_file_name(&backup_name);
    // FlushFileBuffers needs write access on Windows even though this operation never changes
    // the source bytes. Unix permits syncing a read-only descriptor.
    #[cfg(windows)]
    let mut source = fs::OpenOptions::new().read(true).write(true).open(path)?;
    #[cfg(not(windows))]
    let mut source = File::open(path)?;
    source.sync_all()?;
    let thread_id = source_thread_id(&mut source, path)?;
    let (source_bytes, source_sha256) = fingerprint(&mut source)?;
    let manifest = RecoveryManifest {
        version: 1,
        canonical: file_name.to_owned(),
        backup: backup_name,
        format: BackupFormat::PlainJsonl,
        source_bytes,
        source_sha256,
        thread_id,
    };
    fs::hard_link(path, &backup_path)?;
    sync_parent_directory(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{file_name}.media-vacuum-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(source.metadata()?.permissions())?;
    serde_json::to_writer(temporary.as_file_mut(), &manifest)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(manifest_path(path)?)
        .map_err(|error| error.error)?;
    sync_parent_directory(parent)?;
    Ok(backup_path)
}

/// Returns the authorized preimage, never an arbitrary backup or a corrupt canonical substitute.
pub(crate) fn open_recovery_source(path: &Path) -> io::Result<Option<File>> {
    let path = crate::plain_rollout_path(path);
    if path.try_exists()? || crate::compression::compressed_rollout_path(&path).try_exists()? {
        return Ok(None);
    }
    let manifest_path = manifest_path(&path)?;
    let manifest_file = match File::open(&manifest_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !fs::symlink_metadata(&manifest_path)?.file_type().is_file() {
        return Err(io::Error::other(
            "media-vacuum manifest must be a regular file",
        ));
    }
    let mut manifest_bytes = Vec::new();
    manifest_file
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut manifest_bytes)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::other(
            "media-vacuum manifest exceeds its size limit",
        ));
    }
    let manifest: RecoveryManifest =
        serde_json::from_slice(&manifest_bytes).map_err(io::Error::other)?;
    let file_name = canonical_file_name(&path)?;
    if manifest.version != 1
        || manifest.canonical != file_name
        || compacted_media_backup_id(&manifest.backup, file_name).is_none()
    {
        return Err(io::Error::other(
            "media-vacuum manifest does not authorize this rollout",
        ));
    }
    let backup = path.with_file_name(&manifest.backup);
    if !fs::symlink_metadata(&backup)?.file_type().is_file() {
        return Err(io::Error::other(
            "media-vacuum backup must be a regular file",
        ));
    }
    let mut file = File::open(&backup)?;
    let (bytes, sha256) = fingerprint(&mut file)?;
    if bytes != manifest.source_bytes
        || sha256 != manifest.source_sha256
        || source_thread_id(&mut file, &path)? != manifest.thread_id
    {
        return Err(io::Error::other(
            "media-vacuum backup does not match the authorized source",
        ));
    }
    let preflight = preflight_rollout_reader(&mut BufReader::new(&mut file), &path)?;
    if preflight.found_invalid_media_policy_marker
        || !(preflight.found_protected_checkpoint || preflight.found_rewritable_media)
    {
        return Err(io::Error::other(
            "media-vacuum backup has no valid media checkpoint",
        ));
    }
    file.rewind()?;
    Ok(Some(file))
}

/// Restores an authorized source under the caller's existing writer lease.
pub(crate) fn recover_compacted_media_backup_if_needed(path: &Path) -> io::Result<()> {
    let path = crate::plain_rollout_path(path);
    let Some(mut source) = open_recovery_source(&path)? else {
        return Ok(());
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = canonical_file_name(&path)?;
    let mut restored = tempfile::Builder::new()
        .prefix(&format!(".{name}.media-vacuum-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    let metadata = source.metadata()?;
    restored.as_file().set_permissions(metadata.permissions())?;
    io::copy(&mut source, restored.as_file_mut())?;
    if let Ok(modified) = metadata.modified() {
        restored
            .as_file()
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    restored.as_file().sync_all()?;
    restored
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    sync_parent_directory(parent)?;
    // Retain the manifest until explicit cleanup. It is harmless while canonical history exists.
    Ok(())
}

pub(super) fn finish_backup(path: &Path, backup: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(unix)]
    {
        // Retire recovery authority first, so an interrupted cleanup cannot expose stale history.
        fs::remove_file(manifest_path(path)?)?;
        sync_parent_directory(parent)?;
    }
    cleanup_completed_backup(parent, backup)
}

/// Retires a prior vacuum's authority before appending, without scanning ordinary session dirs.
pub(crate) fn retire_recovery_before_append(path: &Path) -> io::Result<()> {
    if manifest_path(path)?.try_exists()? {
        super::remove_compacted_media_vacuum_backups(path)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
