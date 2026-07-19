//! Schema and certification checks before a physical media rewrite.

use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use super::json_spans::JsonSpan;
use super::json_spans::JsonSpanKind;
use super::json_spans::parse_json_spans;
use super::record_validation;
use super::rewrite::compacted_item_contains_rewritable_media;
use super::rewrite::compacted_item_media_content;
use super::rewrite::is_object_type;

const MAX_VACUUM_ROLLOUT_RECORD_BYTES: usize = 256 * 1024 * 1024;
const MEDIA_POLICY_MARKER_FIELD: &str = "replacement_history_media_sanitized_prefix_len";

#[derive(Debug, Default)]
pub(super) struct CompactedMediaVacuumPreflight {
    pub(super) found_valid_rollout_record: bool,
    pub(super) found_protected_checkpoint: bool,
    pub(super) found_invalid_media_policy_marker: bool,
    pub(super) found_rewritable_media: bool,
}

pub(super) fn preflight_rollout(path: &Path) -> io::Result<CompactedMediaVacuumPreflight> {
    let mut reader = BufReader::new(File::open(path)?);
    preflight_rollout_reader(&mut reader, path)
}

pub(super) fn preflight_compressed_rollout(
    path: &Path,
) -> io::Result<CompactedMediaVacuumPreflight> {
    let input = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let mut reader = BufReader::new(decoder);
    preflight_rollout_reader(&mut reader, path)
}

pub(super) fn preflight_rollout_reader(
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

pub(super) fn contains_media_policy_marker(value: &JsonSpan) -> bool {
    // Inspect the payload independently of the outer record type so a syntactically valid but
    // rollout-invalid envelope cannot hide a marker and be reclassified as checkpointless.
    value
        .object_value("payload")
        .is_some_and(|payload| payload.object_value(MEDIA_POLICY_MARKER_FIELD).is_some())
}

pub(super) fn contains_raw_media_policy_marker_key(json: &[u8]) -> bool {
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

pub(super) fn parse_rollout_record(json: &[u8]) -> Option<JsonSpan> {
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

pub(super) fn is_protected_checkpoint(value: &JsonSpan, json: &[u8]) -> bool {
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
                && history[..prefix_len].iter().all(|item| {
                    compacted_item_media_content(item, json).is_none_or(|(content, _)| {
                        content
                            .iter()
                            .all(|item| !is_object_type(item, json, "input_image"))
                    })
                })
        })
}

pub(super) fn contains_rewritable_compacted_media(value: &JsonSpan, json: &[u8]) -> bool {
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

pub(super) fn read_bounded_rollout_record(
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
