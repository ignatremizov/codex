//! Local compaction media retention, preserving source envelopes and atomic fragments.

use super::*;
use codex_utils_output_truncation::approx_token_count;
use std::collections::HashMap;

pub(super) fn build_compacted_history_preserving_mcp_context(
    history_items: &[ResponseItemEnvelope],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<ResponseItemEnvelope> {
    let retained_history =
        collect_annotated_mcp_and_recent_user_items_with_limit(history_items, max_tokens);
    build_compacted_history_with_limit(retained_history, &[], summary_text, /*max_tokens*/ 0)
}

pub(super) fn build_local_compacted_history(
    history_items: &[ResponseItemEnvelope],
    compacted_prefix_len: usize,
    summary_text: &str,
) -> Vec<ResponseItemEnvelope> {
    let mut replacement_history_items = history_items.to_vec();
    let compacted_prefix_len = compacted_prefix_len.min(replacement_history_items.len());
    let mut raw_items = replacement_history_items
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    let _ = crate::context::sanitize_compacted_media_prefix(&mut raw_items, compacted_prefix_len);
    crate::context::expire_compacted_media_references(&mut raw_items[..compacted_prefix_len]);
    let _ = crate::context::sanitize_compacted_media(&mut raw_items[compacted_prefix_len..]);
    for (envelope, item) in replacement_history_items.iter_mut().zip(raw_items) {
        if envelope.item != item {
            if let Some(metadata) = &mut envelope.metadata {
                metadata.mark_retained_sources_incomplete();
            }
            envelope.item = item;
        }
    }
    let current_items = replacement_history_items[compacted_prefix_len..]
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    let current_omission_text = compacted_image_omission_text(&current_items).map(str::to_owned);
    let retained_message_budget = current_omission_text
        .as_deref()
        .map_or(COMPACT_USER_MESSAGE_MAX_TOKENS, |omission| {
            COMPACT_USER_MESSAGE_MAX_TOKENS.saturating_sub(approx_token_count(omission))
        });
    let mut compacted_history = build_compacted_history_preserving_mcp_context(
        replacement_history_items.as_slice(),
        summary_text,
        retained_message_budget,
    );
    let compacted_items = compacted_history
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    if compacted_image_omission_text(&compacted_items).is_none()
        && let Some(omission_text) = current_omission_text
    {
        let insertion_index = compacted_history.len().saturating_sub(1);
        compacted_history.insert(
            insertion_index,
            ResponseItemEnvelope::new(standalone_compacted_image_omission_message(omission_text)),
        );
    }
    compacted_history
}

fn collect_annotated_mcp_and_recent_user_items_with_limit(
    history_items: &[ResponseItemEnvelope],
    max_tokens: usize,
) -> Vec<ResponseItemEnvelope> {
    let mut selected_user_items: HashMap<usize, ResponseItemEnvelope> = HashMap::new();
    let mut remaining = max_tokens;
    if max_tokens > 0 {
        for (index, envelope) in history_items.iter().enumerate().rev() {
            if remaining == 0 {
                break;
            }
            let Some(message) = compacted_user_message(&envelope.item, envelope.metadata.as_ref())
            else {
                continue;
            };
            if is_summary_message(&message.message) {
                continue;
            }
            let tokens = approx_token_count(&message.message);
            let atomic_media = matches!(&envelope.item,
                ResponseItem::Message { content, .. } if contains_atomic_compacted_media(content));
            if tokens <= remaining {
                if atomic_media {
                    selected_user_items.insert(index, envelope.clone());
                } else if let Some(item) =
                    selected_user_messages_with_limit(std::slice::from_ref(&message), remaining)
                        .pop()
                {
                    selected_user_items.insert(index, item);
                }
                remaining = remaining.saturating_sub(tokens);
            } else {
                if atomic_media {
                    match truncate_retained_message_to_token_budget(
                        envelope.item.clone(),
                        remaining,
                    ) {
                        RetainedMessageTruncation::Retained(item) => {
                            let mut metadata = envelope.metadata.clone();
                            if let Some(metadata) = &mut metadata {
                                metadata.mark_retained_sources_incomplete();
                            }
                            selected_user_items.insert(
                                index,
                                ResponseItemEnvelope {
                                    item: *item,
                                    metadata,
                                },
                            );
                        }
                        RetainedMessageTruncation::OmissionDidNotFit
                        | RetainedMessageTruncation::Empty => {}
                    }
                } else {
                    // Charge the generated truncation notice to the budget while retaining
                    // the selected source parts and their positional attribution together.
                    let mut content_budget = remaining;
                    while let Some(item) = selected_user_messages_with_limit(
                        std::slice::from_ref(&message),
                        content_budget,
                    )
                    .pop()
                    {
                        let ResponseItem::Message { content, .. } = &item.item else {
                            break;
                        };
                        let text: String = content
                            .iter()
                            .filter_map(|part| match part {
                                ContentItem::InputText { text }
                                | ContentItem::OutputText { text, .. } => Some(text.as_str()),
                                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                                    None
                                }
                            })
                            .collect();
                        let overflow = approx_token_count(&text).saturating_sub(remaining);
                        if overflow == 0 {
                            selected_user_items.insert(index, item);
                            break;
                        }
                        content_budget = content_budget.saturating_sub(overflow);
                    }
                }
                break;
            }
        }
    }

    history_items
        .iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            if McpServerUseInstructions::matches_response_item(&envelope.item) {
                return Some(envelope.clone());
            }
            selected_user_items.get(&index).cloned()
        })
        .collect()
}
