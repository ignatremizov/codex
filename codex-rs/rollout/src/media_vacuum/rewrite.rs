//! Byte-span media replacements; bytes outside the selected fields remain unchanged.

use std::io;
use std::io::Write;

use super::CompactedMediaVacuumPolicy;
use super::CompactedMediaVacuumReport;
use super::json_spans::JsonSpan;
use super::json_spans::JsonSpanKind;
use super::preflight::is_protected_checkpoint;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;

pub(super) fn compacted_media_replacements(
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

pub(super) fn compacted_item_media_content<'a>(
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

pub(super) fn compacted_item_contains_rewritable_media(item: &JsonSpan, json: &[u8]) -> bool {
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
pub(super) enum ImageReferencePolicy {
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

pub(super) fn is_object_type(value: &JsonSpan, json: &[u8], expected: &str) -> bool {
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

pub(super) fn write_replacements(
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

pub(super) struct ByteReplacement {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}
