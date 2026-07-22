//! Canonical compatibility decoding shared by raw-history consumers.

use serde_json::Value;

use crate::RolloutLine;

/// Decodes one canonical record with the same historical cleanup used by full resume.
///
/// A retired top-level ghost snapshot does not occupy a decoded rollout index. Embedded
/// snapshots are removed together with their metadata, preserving retained-media alignment.
pub fn decode_canonical_rollout_line(mut value: Value) -> serde_json::Result<Option<RolloutLine>> {
    if strip_legacy_ghost_snapshot_rollout_line(&mut value) {
        return Ok(None);
    }
    crate::decode_rollout_line(value).map(Some)
}

pub(crate) fn strip_legacy_ghost_snapshot_rollout_line(value: &mut Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => value
            .get("payload")
            .is_some_and(is_legacy_ghost_snapshot_response_item),
        Some("compacted") => {
            let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) else {
                return false;
            };
            let Some(replacement_history) =
                payload.get("replacement_history").and_then(Value::as_array)
            else {
                return false;
            };
            let remove = replacement_history
                .iter()
                .map(is_legacy_ghost_snapshot_response_item)
                .collect::<Vec<_>>();
            if !remove.contains(&true) {
                return false;
            }

            // Legacy checkpoints have no sidecar. If a sidecar is present, only filter a
            // full-length array; malformed shapes should remain intact for typed deserialization
            // to reject instead of silently shifting metadata onto a different history item.
            match payload.get("replacement_history_metadata") {
                None => {}
                Some(Value::Array(metadata)) if metadata.len() == remove.len() => {}
                Some(_) => return false,
            }

            let Some(replacement_history) = payload
                .get_mut("replacement_history")
                .and_then(Value::as_array_mut)
            else {
                return false;
            };
            retain_entries_not_marked(replacement_history, &remove);
            if let Some(metadata) = payload
                .get_mut("replacement_history_metadata")
                .and_then(Value::as_array_mut)
            {
                retain_entries_not_marked(metadata, &remove);
            }
            false
        }
        _ => false,
    }
}

fn retain_entries_not_marked(entries: &mut Vec<Value>, remove: &[bool]) {
    let mut index = 0;
    entries.retain(|_| {
        let retain = !remove[index];
        index += 1;
        retain
    });
}

fn is_legacy_ghost_snapshot_response_item(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("ghost_snapshot")
}
