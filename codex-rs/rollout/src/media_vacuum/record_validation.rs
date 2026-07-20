use super::json_spans::JsonSpan;
use super::json_spans::JsonSpanKind;

pub(super) fn is_valid_compacted_rollout_record(value: &JsonSpan, json: &[u8]) -> bool {
    is_string_field(value, "timestamp")
        && optional_u64_field_is_valid(value, "ordinal", json)
        && value
            .object_value("type")
            .and_then(|value| value.as_string(json))
            .is_some_and(|item_type| item_type == "compacted")
        && value
            .object_value("payload")
            .is_some_and(|payload| is_valid_compacted_item(payload, json))
}

fn is_valid_compacted_item(value: &JsonSpan, json: &[u8]) -> bool {
    matches!(value.kind, JsonSpanKind::Object(_))
        && is_string_field(value, "message")
        && optional_array_field_is_valid(value, "replacement_history", json)
        && optional_i64_field_is_valid(value, "compaction_summary_tokens", json)
        && optional_u64_field_is_valid(value, "window_number", json)
        && optional_string_field_is_valid(value, "first_window_id", json)
        && optional_string_field_is_valid(value, "previous_window_id", json)
        && optional_window_id_field_is_valid(value, "window_id", json)
        && optional_u64_field_is_valid(
            value,
            "replacement_history_media_sanitized_prefix_len",
            json,
        )
        && optional_bool_field_is_valid(value, "replacement_history_media_repair", json)
}

fn is_string_field(value: &JsonSpan, field: &str) -> bool {
    value
        .object_value(field)
        .is_some_and(|value| matches!(value.kind, JsonSpanKind::String))
}

fn optional_string_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value
        .object_value(field)
        .is_none_or(|value| matches!(value.kind, JsonSpanKind::String) || is_null(value, json))
}

fn optional_array_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value
        .object_value(field)
        .is_none_or(|value| matches!(value.kind, JsonSpanKind::Array(_)) || is_null(value, json))
}

fn optional_i64_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value.object_value(field).is_none_or(|value| {
        is_null(value, json)
            || (matches!(value.kind, JsonSpanKind::Scalar)
                && serde_json::from_slice::<i64>(&json[value.start..value.end]).is_ok())
    })
}

fn optional_u64_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value.object_value(field).is_none_or(|value| {
        is_null(value, json)
            || (matches!(value.kind, JsonSpanKind::Scalar) && value.as_u64(json).is_some())
    })
}

fn optional_window_id_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value.object_value(field).is_none_or(|value| {
        is_null(value, json)
            || matches!(value.kind, JsonSpanKind::String)
            || (matches!(value.kind, JsonSpanKind::Scalar)
                && serde_json::from_slice::<u64>(&json[value.start..value.end]).is_ok())
    })
}

fn optional_bool_field_is_valid(value: &JsonSpan, field: &str, json: &[u8]) -> bool {
    value.object_value(field).is_none_or(|value| {
        matches!(value.kind, JsonSpanKind::Scalar)
            && serde_json::from_slice::<bool>(&json[value.start..value.end]).is_ok()
    })
}

fn is_null(value: &JsonSpan, json: &[u8]) -> bool {
    matches!(value.kind, JsonSpanKind::Scalar) && &json[value.start..value.end] == b"null"
}
