//! Representation-only media retention for a remote compaction replacement.

use super::*;

pub(super) fn build_v2_compacted_history(
    replacement_history_input: &[ResponseItemEnvelope],
    compacted_prefix_len: usize,
    compaction_output: ResponseItem,
    retain_client_developer_messages: bool,
) -> (Vec<ResponseItemEnvelope>, CompactedMediaSanitization) {
    let compacted_prefix_len = compacted_prefix_len.min(replacement_history_input.len());
    let mut inherited = replacement_history_input[..compacted_prefix_len].to_vec();
    let mut current = replacement_history_input[compacted_prefix_len..].to_vec();
    let mut media_sanitization = sanitize_annotated_compacted_media(&mut inherited);
    expire_annotated_compacted_media_references(&mut inherited);
    media_sanitization.accumulate(sanitize_annotated_compacted_media(&mut current));
    let current_omission_text = compacted_image_omission_text_from_envelopes(&current);
    inherited = retain_remote_compaction_v2_items(inherited, retain_client_developer_messages);
    current = retain_remote_compaction_v2_items(current, retain_client_developer_messages);
    if compacted_image_omission_text_from_envelopes(&current).is_none()
        && let Some(omission_text) = current_omission_text.as_ref()
    {
        let target_message = current.iter_mut().rev().find_map(|envelope| {
            if !is_retained_for_remote_compaction_v2(envelope, retain_client_developer_messages) {
                return None;
            }
            matches!(&envelope.item, ResponseItem::Message { .. }).then_some(&mut envelope.item)
        });
        if let Some(message) = target_message {
            let Some(mut content) = codex_context_fragments::to_annotated_content(message) else {
                unreachable!("target_message only returns message items");
            };
            content.push(annotated_compacted_image_omission(omission_text.clone()));
            let _ = codex_context_fragments::set_annotated_content(message, content);
        } else {
            current.push(ResponseItemEnvelope::new(
                standalone_compacted_image_omission_message(omission_text.clone()),
            ));
        }
    }
    inherited.extend(current);
    let mut retained =
        truncate_retained_messages_for_remote_compaction(inherited, RETAINED_MESSAGE_TOKEN_BUDGET);
    if compacted_image_omission_text_from_envelopes(&retained).is_none()
        && let Some(omission_text) = current_omission_text
    {
        retained.push(ResponseItemEnvelope::new(
            standalone_compacted_image_omission_message(omission_text),
        ));
        retained = truncate_retained_messages_for_remote_compaction(
            retained,
            RETAINED_MESSAGE_TOKEN_BUDGET,
        );
    }
    retained.push(ResponseItemEnvelope::new(compaction_output));
    (retained, media_sanitization)
}

fn sanitize_annotated_compacted_media(
    items: &mut [ResponseItemEnvelope],
) -> CompactedMediaSanitization {
    let mut raw_items = items
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    let sanitization = sanitize_compacted_media(&mut raw_items);
    for (envelope, item) in items.iter_mut().zip(raw_items) {
        envelope.item = item;
    }
    sanitization
}

fn expire_annotated_compacted_media_references(items: &mut [ResponseItemEnvelope]) {
    let mut raw_items = items
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    expire_compacted_media_references(&mut raw_items);
    for (envelope, item) in items.iter_mut().zip(raw_items) {
        envelope.item = item;
    }
}

fn compacted_image_omission_text_from_envelopes(items: &[ResponseItemEnvelope]) -> Option<String> {
    let raw_items = items
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    compacted_image_omission_text(&raw_items).map(str::to_owned)
}

fn retain_remote_compaction_v2_items(
    items: Vec<ResponseItemEnvelope>,
    retain_client_developer_messages: bool,
) -> Vec<ResponseItemEnvelope> {
    v2_history_item_groups(items)
        .filter(|group| {
            is_retained_for_remote_compaction_v2(&group.source, retain_client_developer_messages)
        })
        .filter(|group| {
            !matches!(
                &group.source.item,
                ResponseItem::Message { content, .. } if content.is_empty()
            )
        })
        .flat_map(HistoryItemGroup::into_items)
        .collect()
}
