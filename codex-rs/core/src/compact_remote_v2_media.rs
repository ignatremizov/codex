//! Representation-only media retention for a remote compaction replacement.

use super::*;

#[cfg(test)]
pub(super) fn build_v2_compacted_history(
    input: &[ResponseItemEnvelope],
    compacted_prefix_len: usize,
    compaction_output: ResponseItem,
    features: &Features,
) -> (Vec<ResponseItemEnvelope>, CompactedMediaSanitization) {
    build_v2_compacted_history_with_audio(
        input,
        compacted_prefix_len,
        compaction_output,
        features,
        &FileAudioDurations::default(),
    )
}

pub(super) fn build_v2_compacted_history_with_audio(
    replacement_history_input: &[ResponseItemEnvelope],
    compacted_prefix_len: usize,
    compaction_output: ResponseItem,
    features: &Features,
    audio_durations: &FileAudioDurations,
) -> (Vec<ResponseItemEnvelope>, CompactedMediaSanitization) {
    let no_file_error_sources =
        crate::context_manager::rejected_audio::checkpoint_input_has_no_file_error_sources(
            replacement_history_input,
        );
    let pending_sequence =
        crate::compact_pending_input::pending_mail_sequence(replacement_history_input);
    let compacted_prefix_len = compacted_prefix_len.min(replacement_history_input.len());
    let mut inherited = replacement_history_input[..compacted_prefix_len].to_vec();
    let mut current = replacement_history_input[compacted_prefix_len..].to_vec();
    let mut media_sanitization = sanitize_annotated_compacted_media(&mut inherited);
    expire_annotated_compacted_media_references(&mut inherited);
    media_sanitization.accumulate(sanitize_annotated_compacted_media(&mut current));
    let current_omission_text = compacted_image_omission_text_from_envelopes(&current);
    inherited = retain_remote_compaction_v2_items(inherited, features, pending_sequence);
    current = retain_remote_compaction_v2_items(current, features, pending_sequence);
    if compacted_image_omission_text_from_envelopes(&current).is_none()
        && let Some(omission_text) = current_omission_text.as_ref()
    {
        let target_message = current.iter_mut().rev().find_map(|envelope| {
            if !is_retained_for_remote_compaction_v2(envelope, features) {
                return None;
            }
            matches!(&envelope.item, ResponseItem::Message { .. }).then_some(envelope)
        });
        if let Some(envelope) = target_message {
            let Some(mut content) =
                codex_context_fragments::to_annotated_content(&mut envelope.item)
            else {
                unreachable!("target_message only returns message items");
            };
            content.push(annotated_compacted_image_omission(omission_text.clone()));
            let _ = codex_context_fragments::set_annotated_content(&mut envelope.item, content);
            if let Some(metadata) = &mut envelope.metadata {
                metadata.mark_retained_sources_incomplete();
            }
        } else {
            current.push(ResponseItemEnvelope::new(
                standalone_compacted_image_omission_message(omission_text.clone()),
            ));
        }
    }
    inherited.extend(current);
    let mut retained = truncate_retained_messages_for_remote_compaction_with_audio(
        inherited,
        RETAINED_MESSAGE_TOKEN_BUDGET,
        audio_durations,
    );
    if compacted_image_omission_text_from_envelopes(&retained).is_none()
        && let Some(omission_text) = current_omission_text
    {
        retained.push(ResponseItemEnvelope::new(
            standalone_compacted_image_omission_message(omission_text),
        ));
        retained = truncate_retained_messages_for_remote_compaction_with_audio(
            retained,
            RETAINED_MESSAGE_TOKEN_BUDGET,
            audio_durations,
        );
    }
    let mut checkpoint = ResponseItemEnvelope::new(compaction_output);
    if no_file_error_sources {
        checkpoint
            .metadata
            .get_or_insert_default()
            .compaction_without_file_error_sources = true;
    }
    let retained =
        crate::compact_pending_input::insert_summary(retained, checkpoint, pending_sequence);
    (retained, media_sanitization)
}

fn sanitize_annotated_compacted_media(
    items: &mut [ResponseItemEnvelope],
) -> CompactedMediaSanitization {
    let prefix_len = items.len();
    crate::context::sanitize_compacted_media_envelopes(items, prefix_len)
}

fn expire_annotated_compacted_media_references(items: &mut [ResponseItemEnvelope]) {
    let mut raw_items = items
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    expire_compacted_media_references(&mut raw_items);
    for (envelope, item) in items.iter_mut().zip(raw_items) {
        if envelope.item != item {
            if let Some(metadata) = &mut envelope.metadata {
                metadata.mark_retained_sources_incomplete();
            }
            envelope.item = item;
        }
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
    features: &Features,
    pending_sequence: Option<u64>,
) -> Vec<ResponseItemEnvelope> {
    v2_history_item_groups(items)
        .filter(|group| {
            is_retained_for_remote_compaction_v2(&group.source, features)
                || pending_sequence
                    .is_some_and(|first| is_retained_pending_mail(&group.source, first))
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
