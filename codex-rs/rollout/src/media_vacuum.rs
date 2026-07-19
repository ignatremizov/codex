use std::fs;
use std::fs::File;
use std::fs::FileTimes;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;
use tracing::warn;
use uuid::Uuid;

use self::json_spans::JsonSpan;
use self::json_spans::JsonSpanKind;
use self::json_spans::parse_json_spans;

mod json_spans;
mod record_validation;

const MAX_VACUUM_ROLLOUT_RECORD_BYTES: usize = 256 * 1024 * 1024;
const MEDIA_POLICY_MARKER_FIELD: &str = "replacement_history_media_sanitized_prefix_len";

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
/// The caller must ensure no writer has the rollout open. Legacy rollouts without a media-policy
/// marker are repaired directly by sanitizing every valid compacted replacement history. Once a
/// rollout contains a marker, every marked record must be a valid sanitized checkpoint so its
/// potentially unsummarized suffix remains protected.
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
                restore_compacted_media_backup(backup_path.as_path(), path, parent)?;
            } else {
                let _ = fs::remove_file(backup_path.as_path());
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
        if validate_rollout_for_vacuum_recovery(backup_path.as_path())
            .is_ok_and(std::convert::identity)
        {
            match restore_compacted_media_backup(backup_path.as_path(), path, parent) {
                Ok(()) => return Ok(()),
                Err(_)
                    if validate_rollout_for_vacuum_recovery(path)
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

fn validate_rollout_for_vacuum_recovery(path: &Path) -> io::Result<bool> {
    let preflight = preflight_rollout(path)?;
    // A checkpointless recovery backup is the pre-rewrite file, so rewritable compacted media is
    // the evidence that it is a valid vacuum source rather than an unrelated rollout-shaped file.
    Ok(!preflight.found_invalid_media_policy_marker
        && (preflight.found_protected_checkpoint || preflight.found_rewritable_media))
}

#[derive(Debug, Default)]
struct CompactedMediaVacuumPreflight {
    found_valid_rollout_record: bool,
    found_protected_checkpoint: bool,
    found_invalid_media_policy_marker: bool,
    found_rewritable_media: bool,
}

fn preflight_rollout(path: &Path) -> io::Result<CompactedMediaVacuumPreflight> {
    let mut reader = BufReader::new(File::open(path)?);
    preflight_rollout_reader(&mut reader, path)
}

fn preflight_compressed_rollout(path: &Path) -> io::Result<CompactedMediaVacuumPreflight> {
    let input = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let mut reader = BufReader::new(decoder);
    preflight_rollout_reader(&mut reader, path)
}

fn preflight_rollout_reader(
    reader: &mut impl BufRead,
    path: &Path,
) -> io::Result<CompactedMediaVacuumPreflight> {
    let mut preflight = CompactedMediaVacuumPreflight::default();
    let mut line = Vec::new();
    while read_bounded_rollout_record(reader, &mut line, path)? {
        let json = line.strip_suffix(b"\n").unwrap_or(line.as_slice());
        let json = json.strip_suffix(b"\r").unwrap_or(json);
        if !json.iter().all(u8::is_ascii_whitespace) {
            match parse_rollout_record(json) {
                Some(spans) => {
                    let contains_raw_marker = contains_raw_media_policy_marker_key(json);
                    let validate_schema =
                        !preflight.found_valid_rollout_record || contains_raw_marker;
                    let schema_valid = if is_object_type(&spans, json, "compacted") {
                        // Successful compacted parsing already performed the complete rollout
                        // schema check, including media-string elision.
                        Some(true)
                    } else if validate_schema {
                        Some(
                            record_validation::is_valid_rollout_record_without_materializing_inline_media(
                                &spans, json,
                            ),
                        )
                    } else {
                        None
                    };
                    preflight.found_valid_rollout_record |= schema_valid.unwrap_or(false);
                    if contains_media_policy_marker(&spans) {
                        if is_protected_checkpoint(&spans, json) {
                            preflight.found_protected_checkpoint = true;
                        } else {
                            preflight.found_invalid_media_policy_marker = true;
                        }
                    } else if contains_raw_marker && schema_valid == Some(false) {
                        preflight.found_invalid_media_policy_marker = true;
                    } else if contains_rewritable_compacted_media(&spans, json) {
                        preflight.found_rewritable_media = true;
                    }
                }
                None => {
                    preflight.found_invalid_media_policy_marker |=
                        contains_raw_media_policy_marker_key(json);
                }
            }
        }
        line.clear();
    }
    Ok(preflight)
}

fn contains_media_policy_marker(value: &JsonSpan) -> bool {
    // Inspect the payload independently of the outer record type so a syntactically valid but
    // rollout-invalid envelope cannot hide a marker and be reclassified as checkpointless.
    value
        .object_value("payload")
        .is_some_and(|payload| payload.object_value(MEDIA_POLICY_MARKER_FIELD).is_some())
}

fn contains_raw_media_policy_marker_key(json: &[u8]) -> bool {
    let marker = MEDIA_POLICY_MARKER_FIELD.as_bytes();
    'candidate: for (opening_quote, byte) in json.iter().enumerate() {
        if *byte != b'"' {
            continue;
        }
        let mut cursor = opening_quote.saturating_add(1);
        for expected in marker {
            match json.get(cursor) {
                Some(actual) if actual == expected => cursor = cursor.saturating_add(1),
                Some(b'\\') if json.get(cursor.saturating_add(1)) == Some(&b'u') => {
                    let Some(hex) = json.get(cursor.saturating_add(2)..cursor.saturating_add(6))
                    else {
                        continue 'candidate;
                    };
                    let mut code_unit = 0u16;
                    for byte in hex {
                        let digit = match *byte {
                            b'0'..=b'9' => *byte - b'0',
                            b'a'..=b'f' => *byte - b'a' + 10,
                            b'A'..=b'F' => *byte - b'A' + 10,
                            _ => continue 'candidate,
                        };
                        code_unit = code_unit
                            .saturating_mul(16)
                            .saturating_add(u16::from(digit));
                    }
                    if code_unit != u16::from(*expected) {
                        continue 'candidate;
                    }
                    cursor = cursor.saturating_add(6);
                }
                Some(_) | None => continue 'candidate,
            }
        }
        if json.get(cursor) != Some(&b'"') {
            continue;
        }
        return true;
    }
    false
}

fn parse_rollout_record(json: &[u8]) -> Option<JsonSpan> {
    // Validate the complete JSON value without materializing large inline-media strings. The span
    // parser below then locates only the fields needed for the targeted rewrite.
    let spans = parse_json_spans(json).ok()?;
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    if <serde::de::IgnoredAny as serde::Deserialize>::deserialize(&mut deserializer).is_err()
        || deserializer.end().is_err()
    {
        return None;
    }
    if !matches!(&spans.kind, JsonSpanKind::Object(_))
        || !spans
            .object_value("timestamp")
            .is_some_and(|value| matches!(&value.kind, JsonSpanKind::String))
        || !spans
            .object_value("type")
            .is_some_and(|value| matches!(&value.kind, JsonSpanKind::String))
        || spans.object_value("payload").is_none()
    {
        return None;
    }
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
            let Some(prefix_len) = payload
                .object_value("replacement_history_media_sanitized_prefix_len")
                .and_then(|value| value.as_u64(json))
                .and_then(|prefix_len| usize::try_from(prefix_len).ok())
            else {
                return false;
            };
            let Some(history) = payload
                .object_value("replacement_history")
                .and_then(JsonSpan::as_array)
            else {
                return false;
            };
            prefix_len <= history.len()
                && history[..prefix_len]
                    .iter()
                    .all(|item| !compacted_item_contains_rewritable_media(item, json))
        })
}

fn contains_rewritable_compacted_media(value: &JsonSpan, json: &[u8]) -> bool {
    if !is_object_type(value, json, "compacted") || is_protected_checkpoint(value, json) {
        return false;
    }
    value
        .object_value("payload")
        .and_then(|payload| payload.object_value("replacement_history"))
        .and_then(JsonSpan::as_array)
        .is_some_and(|history| {
            history
                .iter()
                .any(|item| compacted_item_contains_rewritable_media(item, json))
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

    let mut images = Vec::new();
    let mut existing_omissions = Vec::new();
    for item in history {
        let Some((content, reference_policy)) = compacted_item_media_content(item, json) else {
            continue;
        };
        append_content_media_candidates(
            content,
            json,
            reference_policy,
            policy,
            &mut images,
            &mut existing_omissions,
        );
    }
    let Some(marker_index) = images.len().checked_sub(1) else {
        return Ok(Vec::new());
    };
    let has_local_reference = images.iter().any(|image| image.has_local_reference)
        || existing_omissions
            .iter()
            .any(|omission| omission.has_local_reference);
    let has_unavailable = images.iter().any(|image| !image.has_local_reference)
        || existing_omissions
            .iter()
            .any(|omission| omission.has_unavailable);
    let omission = match (has_local_reference, has_unavailable) {
        (true, true) => policy.mixed_image_omission.as_str(),
        (true, false) => policy.reopenable_image_omission.as_str(),
        (false, true) => policy.unavailable_image_omission.as_str(),
        (false, false) => policy.unavailable_image_omission.as_str(),
    };
    let omission_replacement = serde_json::to_vec(&InputTextReplacement {
        item_type: "input_text",
        text: omission,
    })?;
    let neutral_replacement = serde_json::to_vec(&InputTextReplacement {
        item_type: "input_text",
        text: "",
    })?;
    let mut replacements = images
        .into_iter()
        .enumerate()
        .map(|(index, image)| {
            report.omitted_image_count = report.omitted_image_count.saturating_add(1);
            report.omitted_inline_media_bytes = report
                .omitted_inline_media_bytes
                .saturating_add(image.image_url_bytes);
            ByteReplacement {
                start: image.start,
                end: image.end,
                replacement: if index == marker_index {
                    omission_replacement.clone()
                } else {
                    neutral_replacement.clone()
                },
            }
        })
        .collect::<Vec<_>>();
    replacements.extend(
        existing_omissions
            .into_iter()
            .map(|omission| ByteReplacement {
                start: omission.start,
                end: omission.end,
                replacement: neutral_replacement.clone(),
            }),
    );
    replacements.sort_unstable_by_key(|replacement| replacement.start);
    Ok(replacements)
}

fn compacted_item_media_content<'a>(
    item: &'a JsonSpan,
    json: &[u8],
) -> Option<(&'a [JsonSpan], ImageReferencePolicy)> {
    if is_object_type(item, json, "message") {
        item.object_value("content")
            .and_then(JsonSpan::as_array)
            .map(|content| (content, ImageReferencePolicy::CanonicalLocalWrapper))
    } else if is_object_type(item, json, "function_call_output")
        || is_object_type(item, json, "custom_tool_call_output")
    {
        item.object_value("output")
            .and_then(JsonSpan::as_array)
            .map(|output| (output, ImageReferencePolicy::Unavailable))
    } else {
        None
    }
}

fn compacted_item_contains_rewritable_media(item: &JsonSpan, json: &[u8]) -> bool {
    compacted_item_media_content(item, json).is_some_and(|(content, _)| {
        content
            .iter()
            .any(|item| rewritable_image_url(item, json).is_some())
    })
}

fn rewritable_image_url<'a>(item: &'a JsonSpan, json: &[u8]) -> Option<&'a JsonSpan> {
    if !is_object_type(item, json, "input_image") {
        return None;
    }
    item.object_value("image_url")
        .filter(|image_url| matches!(&image_url.kind, JsonSpanKind::String))
}

fn append_content_media_candidates(
    content: &[JsonSpan],
    json: &[u8],
    reference_policy: ImageReferencePolicy,
    policy: &CompactedMediaVacuumPolicy,
    images: &mut Vec<CompactedImageCandidate>,
    existing_omissions: &mut Vec<CompactedOmissionCandidate>,
) {
    for index in 0..content.len() {
        let image = &content[index];
        let Some(image_url) = rewritable_image_url(image, json) else {
            if let Some(text) = content_item_text(image, json) {
                let (has_local_reference, has_unavailable) =
                    if text.as_str() == policy.reopenable_image_omission.as_str() {
                        (true, false)
                    } else if text.as_str() == policy.unavailable_image_omission.as_str() {
                        (false, true)
                    } else if text.as_str() == policy.mixed_image_omission.as_str() {
                        (true, true)
                    } else {
                        continue;
                    };
                existing_omissions.push(CompactedOmissionCandidate {
                    start: image.start,
                    end: image.end,
                    has_local_reference,
                    has_unavailable,
                });
            }
            continue;
        };
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
        images.push(CompactedImageCandidate {
            start: image.start,
            end: image.end,
            image_url_bytes,
            has_local_reference,
        });
    }
}

#[derive(Clone, Copy)]
enum ImageReferencePolicy {
    CanonicalLocalWrapper,
    Unavailable,
}

struct CompactedImageCandidate {
    start: usize,
    end: usize,
    image_url_bytes: u64,
    has_local_reference: bool,
}

struct CompactedOmissionCandidate {
    start: usize,
    end: usize,
    has_local_reference: bool,
    has_unavailable: bool,
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

/// Removes recoverable backups and interrupted temporary files associated with `path`.
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
    let temporary_prefix = format!(".{file_name}.media-vacuum-");
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

fn compacted_media_backup_id(name: &str, file_name: &str) -> Option<Uuid> {
    let (backup_file_name, backup_id) = compacted_media_backup_parts(name)?;
    (backup_file_name == file_name).then_some(backup_id)
}

/// Returns the canonical rollout file name encoded by a media-vacuum backup name.
pub(crate) fn compacted_media_backup_rollout_file_name(name: &str) -> Option<&str> {
    compacted_media_backup_parts(name).map(|(file_name, _)| file_name)
}

/// Returns the canonical rollout file name encoded by any media-vacuum artifact name.
pub fn compacted_media_vacuum_artifact_rollout_file_name(name: &str) -> Option<&str> {
    compacted_media_backup_rollout_file_name(name).or_else(|| {
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
fn restore_compacted_media_backup(
    backup_path: &Path,
    path: &Path,
    parent: &Path,
) -> io::Result<()> {
    fs::hard_link(backup_path, path)?;
    sync_parent_directory(parent)?;
    cleanup_completed_backup(parent, backup_path)
}

#[cfg(not(unix))]
fn restore_compacted_media_backup(
    backup_path: &Path,
    path: &Path,
    _parent: &Path,
) -> io::Result<()> {
    // Preserve the recovery name until a later explicit cleanup because this platform has no
    // directory durability barrier for the newly linked canonical name.
    fs::hard_link(backup_path, path)
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
pub(crate) fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "media_vacuum_tests.rs"]
mod tests;
