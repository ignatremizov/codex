use codex_protocol::protocol::RolloutLine;

use super::json_spans::JsonSpan;
use super::json_spans::JsonSpanKind;

pub(super) fn is_valid_rollout_record_without_materializing_inline_media(
    value: &JsonSpan,
    json: &[u8],
) -> bool {
    let mut inline_media_spans = Vec::new();
    collect_inline_media_spans(value, json, &mut inline_media_spans);
    if inline_media_spans.is_empty() {
        return serde_json::from_slice::<RolloutLine>(json).is_ok();
    }

    inline_media_spans.sort_unstable_by_key(|(start, _)| *start);
    let mut validation_json = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in inline_media_spans {
        if start < cursor || end > json.len() {
            return false;
        }
        validation_json.extend_from_slice(&json[cursor..start]);
        validation_json.extend_from_slice(b"\"\"");
        cursor = end;
    }
    validation_json.extend_from_slice(&json[cursor..]);
    serde_json::from_slice::<RolloutLine>(validation_json.as_slice()).is_ok()
}

fn collect_inline_media_spans(
    value: &JsonSpan,
    json: &[u8],
    inline_media_spans: &mut Vec<(usize, usize)>,
) {
    match &value.kind {
        JsonSpanKind::Object(fields) => {
            if value
                .object_value("type")
                .and_then(|value| value.as_string(json))
                .is_some_and(|item_type| item_type == "input_image")
                && let Some(image_url) = value.object_value("image_url")
                && matches!(image_url.kind, JsonSpanKind::String)
            {
                inline_media_spans.push((image_url.start, image_url.end));
            }
            for child in fields.values() {
                collect_inline_media_spans(child, json, inline_media_spans);
            }
        }
        JsonSpanKind::Array(items) => {
            for item in items {
                collect_inline_media_spans(item, json, inline_media_spans);
            }
        }
        JsonSpanKind::String | JsonSpanKind::Scalar => {}
    }
}
