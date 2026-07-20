use std::fs;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;
use tempfile::NamedTempFile;
use tracing::warn;
use uuid::Uuid;

use self::json_spans::JsonSpan;
use self::json_spans::JsonSpanKind;
use self::json_spans::parse_json_spans;

mod json_spans;
mod record_validation;

const MAX_VACUUM_ROLLOUT_RECORD_BYTES: usize = 256 * 1024 * 1024;

/// Bounded replacement text used while vacuuming historic checkpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedMediaVacuumPolicy {
    /// Replacement for an image that retains a canonical local source-path wrapper.
    pub reopenable_image_omission: String,
    /// Replacement for an image without a durable source reference.
    pub unavailable_image_omission: String,
}

/// Storage reduction and media-omission totals from a completed rollout vacuum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactedMediaVacuumReport {
    /// Physical rollout size before replacement.
    pub bytes_before: u64,
    /// Physical rollout size after replacement.
    pub bytes_after: u64,
    /// Number of superseded compacted JSONL records rewritten.
    pub records_rewritten: usize,
    /// Number of inline image items replaced.
    pub omitted_image_count: usize,
    /// Serialized image URL bytes replaced.
    pub omitted_inline_media_bytes: u64,
}

/// Rewrites a closed rollout to remove media from superseded compacted checkpoints.
///
/// The caller must ensure no writer has the rollout open. The file must already contain a marked
/// sanitized checkpoint produced by reconstruction or compaction.
pub fn vacuum_compacted_media(
    path: &Path,
    policy: &CompactedMediaVacuumPolicy,
) -> io::Result<CompactedMediaVacuumReport> {
    let metadata = fs::metadata(path)?;
    if !validate_rollout_and_find_protected_checkpoint(path)? {
        return Err(io::Error::other(
            "refusing compacted-media vacuum without a sanitized replacement-history checkpoint",
        ));
    }
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
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(metadata.permissions())?;

    let mut report = CompactedMediaVacuumReport {
        bytes_before: metadata.len(),
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
    temporary.as_file().sync_all()?;
    let rewritten_len = temporary.as_file().metadata()?.len();

    if report.records_rewritten == 0 {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }

    let backup_path = path.with_file_name(format!(
        ".{file_name}.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    fs::hard_link(path, backup_path.as_path())?;
    sync_parent_directory(parent)?;
    match temporary.persist(path) {
        Ok(_) => {}
        Err(err) => {
            if !path.exists() {
                fs::rename(backup_path.as_path(), path)?;
            } else {
                let _ = fs::remove_file(backup_path.as_path());
            }
            return Err(err.error);
        }
    }
    report.bytes_after = rewritten_len;
    if let Err(err) = sync_parent_directory(parent) {
        // Replacement has committed. Keep the recoverable backup and report success so callers
        // still rebuild derived projections against the selected canonical file.
        warn!(
            %err,
            path = %path.display(),
            "failed to sync rollout directory after compacted-media vacuum"
        );
        return Ok(report);
    }
    if let Err(err) = cleanup_completed_backup(parent, backup_path.as_path()) {
        warn!(
            %err,
            path = %path.display(),
            "failed to clean up compacted-media vacuum backup"
        );
    }
    Ok(report)
}

pub(crate) fn recover_compacted_media_backup_if_needed(path: &Path) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(backup_id) = name
            .to_str()
            .and_then(|name| compacted_media_backup_id(name, file_name))
        else {
            continue;
        };
        candidates.push((backup_id, entry.path()));
    }
    candidates.sort_by(|(left, _), (right, _)| right.cmp(left));
    for (_, backup_path) in candidates {
        if validate_rollout_and_find_protected_checkpoint(backup_path.as_path())
            .is_ok_and(std::convert::identity)
        {
            match fs::rename(backup_path, path) {
                Ok(()) => {
                    sync_parent_directory(parent)?;
                    return Ok(());
                }
                Err(_)
                    if validate_rollout_and_find_protected_checkpoint(path)
                        .is_ok_and(std::convert::identity) =>
                {
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

fn validate_rollout_and_find_protected_checkpoint(path: &Path) -> io::Result<bool> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut found_protected_checkpoint = false;
    let mut line = Vec::new();
    while read_bounded_rollout_record(&mut reader, &mut line, path)? {
        let json = line.strip_suffix(b"\n").unwrap_or(line.as_slice());
        let json = json.strip_suffix(b"\r").unwrap_or(json);
        if !json.iter().all(u8::is_ascii_whitespace) {
            match parse_rollout_record(json) {
                Some(spans) if is_protected_checkpoint(&spans, json) => {
                    found_protected_checkpoint = true;
                }
                Some(_) | None => {}
            }
        }
        line.clear();
    }
    Ok(found_protected_checkpoint)
}

fn parse_rollout_record(json: &[u8]) -> Option<JsonSpan> {
    // Validate the complete JSON value without materializing large inline-media strings. The span
    // parser below then locates only the fields needed for the targeted rewrite.
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    if <serde::de::IgnoredAny as serde::Deserialize>::deserialize(&mut deserializer).is_err()
        || deserializer.end().is_err()
    {
        return None;
    }
    let spans = parse_json_spans(json).ok()?;
    if is_object_type(&spans, json, "compacted")
        && !record_validation::is_valid_rollout_record_without_materializing_inline_media(
            &spans, json,
        )
    {
        return None;
    }
    Some(spans)
}

fn is_protected_checkpoint(value: &JsonSpan, json: &[u8]) -> bool {
    is_object_type(value, json, "compacted")
        && value.object_value("payload").is_some_and(|payload| {
            payload
                .object_value("replacement_history_media_sanitized_prefix_len")
                .is_some_and(|value| value.as_u64(json).is_some())
                && payload
                    .object_value("replacement_history")
                    .is_some_and(|value| value.as_array().is_some())
        })
}

fn compacted_media_replacements(
    value: &JsonSpan,
    json: &[u8],
    policy: &CompactedMediaVacuumPolicy,
    report: &mut CompactedMediaVacuumReport,
) -> io::Result<Vec<ByteReplacement>> {
    if is_protected_checkpoint(value, json) {
        // Every marked checkpoint remains a possible rollback-selected base. Its prefix is already
        // certified media-free, while its post-prefix suffix may be the only persisted copy of
        // unsummarized media, so preserving only the newest marker would change replay semantics.
        return Ok(Vec::new());
    }
    if !is_object_type(value, json, "compacted") {
        return Ok(Vec::new());
    }
    let Some(payload) = value.object_value("payload") else {
        return Ok(Vec::new());
    };
    let Some(history) = payload
        .object_value("replacement_history")
        .and_then(JsonSpan::as_array)
    else {
        return Ok(Vec::new());
    };

    let mut replacements = Vec::new();
    for item in history {
        let (content, reference_policy) = if is_object_type(item, json, "message") {
            (
                item.object_value("content").and_then(JsonSpan::as_array),
                ImageReferencePolicy::CanonicalLocalWrapper,
            )
        } else if is_object_type(item, json, "function_call_output")
            || is_object_type(item, json, "custom_tool_call_output")
        {
            (
                item.object_value("output").and_then(JsonSpan::as_array),
                ImageReferencePolicy::Unavailable,
            )
        } else {
            (None, ImageReferencePolicy::Unavailable)
        };
        if let Some(content) = content {
            append_content_media_replacements(
                content,
                json,
                reference_policy,
                policy,
                report,
                &mut replacements,
            )?;
        }
    }
    Ok(replacements)
}

fn append_content_media_replacements(
    content: &[JsonSpan],
    json: &[u8],
    reference_policy: ImageReferencePolicy,
    policy: &CompactedMediaVacuumPolicy,
    report: &mut CompactedMediaVacuumReport,
    replacements: &mut Vec<ByteReplacement>,
) -> io::Result<()> {
    for index in 0..content.len() {
        let image = &content[index];
        if !is_object_type(image, json, "input_image") {
            continue;
        }
        let Some(image_url) = image.object_value("image_url") else {
            continue;
        };
        if !matches!(&image_url.kind, JsonSpanKind::String) {
            continue;
        }
        let image_url_bytes = u64::try_from(
            image_url
                .end
                .saturating_sub(image_url.start)
                .saturating_sub(2),
        )
        .unwrap_or(u64::MAX);
        let has_local_reference = matches!(
            reference_policy,
            ImageReferencePolicy::CanonicalLocalWrapper
        ) && index > 0
            && content_item_text(&content[index - 1], json)
                .is_some_and(|text| is_local_image_open_tag_with_path_text(text.as_str()))
            && content
                .get(index + 1)
                .and_then(|value| content_item_text(value, json))
                .is_some_and(|text| is_local_image_close_tag_text(text.as_str()));
        let omission = if has_local_reference {
            policy.reopenable_image_omission.as_str()
        } else {
            policy.unavailable_image_omission.as_str()
        };
        let replacement = serde_json::to_vec(&InputTextReplacement {
            item_type: "input_text",
            text: omission,
        })?;
        replacements.push(ByteReplacement {
            start: image.start,
            end: image.end,
            replacement,
        });
        report.omitted_image_count = report.omitted_image_count.saturating_add(1);
        report.omitted_inline_media_bytes = report
            .omitted_inline_media_bytes
            .saturating_add(image_url_bytes);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ImageReferencePolicy {
    CanonicalLocalWrapper,
    Unavailable,
}

fn read_bounded_rollout_record(
    reader: &mut impl BufRead,
    record: &mut Vec<u8>,
    path: &Path,
) -> io::Result<bool> {
    record.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!record.is_empty());
        }
        let newline_index = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline_index.map_or(available.len(), |index| index.saturating_add(1));
        if record.len().saturating_add(consumed) > MAX_VACUUM_ROLLOUT_RECORD_BYTES {
            return Err(io::Error::other(format!(
                "rollout record in {} exceeds the {} byte compacted-media vacuum limit",
                path.display(),
                MAX_VACUUM_ROLLOUT_RECORD_BYTES
            )));
        }
        record.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline_index.is_some() {
            return Ok(true);
        }
    }
}

/// Removes recoverable compacted-media vacuum backups associated with `path`.
///
/// Thread deletion and archival call this after validating that `path` belongs to the selected
/// thread so a retained backup cannot later resurrect a deleted or moved rollout.
pub fn remove_compacted_media_vacuum_backups(path: &Path) -> io::Result<()> {
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
        if compacted_media_backup_id(name, file_name).is_some() {
            fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        sync_parent_directory(parent)?;
    }
    Ok(())
}

fn compacted_media_backup_id(name: &str, file_name: &str) -> Option<Uuid> {
    let (backup_file_name, backup_id) = compacted_media_backup_parts(name)?;
    (backup_file_name == file_name).then_some(backup_id)
}

pub(crate) fn compacted_media_backup_rollout_file_name(name: &str) -> Option<&str> {
    compacted_media_backup_parts(name).map(|(file_name, _)| file_name)
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
fn cleanup_completed_backup(parent: &Path, backup_path: &Path) -> io::Result<()> {
    fs::remove_file(backup_path)?;
    sync_parent_directory(parent)
}

#[cfg(not(unix))]
fn cleanup_completed_backup(_parent: &Path, _backup_path: &Path) -> io::Result<()> {
    // Keep the recovery link when the platform cannot provide a directory durability barrier.
    // The next manual vacuum, archive, or delete operation removes retained backups explicitly.
    Ok(())
}

fn is_object_type(value: &JsonSpan, json: &[u8], expected: &str) -> bool {
    value
        .object_value("type")
        .and_then(|value| value.as_string(json))
        .is_some_and(|value| value == expected)
}

fn content_item_text(value: &JsonSpan, json: &[u8]) -> Option<String> {
    is_object_type(value, json, "input_text")
        .then(|| value.object_value("text")?.as_string(json))
        .flatten()
}

fn write_replacements(
    writer: &mut impl Write,
    json: &[u8],
    replacements: &[ByteReplacement],
) -> io::Result<()> {
    let mut cursor = 0usize;
    for replacement in replacements {
        if replacement.start < cursor || replacement.end > json.len() {
            return Err(io::Error::other(
                "compacted-media replacement spans overlap or exceed their rollout record",
            ));
        }
        writer.write_all(&json[cursor..replacement.start])?;
        writer.write_all(replacement.replacement.as_slice())?;
        cursor = replacement.end;
    }
    writer.write_all(&json[cursor..])
}

#[derive(serde::Serialize)]
struct InputTextReplacement<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    text: &'a str,
}

struct ByteReplacement {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "media_vacuum_tests.rs"]
mod tests;
