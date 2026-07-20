use std::collections::VecDeque;

use crate::context::is_compacted_image_omission_text;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

pub(crate) enum RetainedMessageTruncation {
    Retained(Box<ResponseItem>),
    OmissionDidNotFit,
    Empty,
}

pub(crate) fn truncate_retained_message_to_token_budget(
    item: ResponseItem,
    max_tokens: usize,
) -> RetainedMessageTruncation {
    let ResponseItem::Message {
        id,
        role,
        content,
        phase,
        internal_chat_message_metadata_passthrough: metadata,
    } = item
    else {
        return RetainedMessageTruncation::Retained(Box::new(item));
    };

    let mut remaining_content = VecDeque::from(content);
    let mut remaining = max_tokens;
    let mut truncated_content = Vec::with_capacity(remaining_content.len());
    while let Some(mut content_item) = remaining_content.pop_front() {
        if matches!(
            &content_item,
            ContentItem::InputText { text }
                if is_local_image_open_tag_with_path_text(text)
        ) {
            let (wrapper_tail_len, wrapper_has_omission) =
                match (remaining_content.front(), remaining_content.get(1)) {
                    (Some(ContentItem::InputText { text }), _)
                        if is_local_image_close_tag_text(text) =>
                    {
                        (1usize, false)
                    }
                    (
                        Some(ContentItem::InputText { text: omission }),
                        Some(ContentItem::InputText { text: close }),
                    ) if is_compacted_image_omission_text(omission)
                        && is_local_image_close_tag_text(close) =>
                    {
                        (2usize, true)
                    }
                    _ => (0usize, false),
                };
            if wrapper_tail_len > 0 {
                let wrapper_tokens = std::iter::once(&content_item)
                    .chain(remaining_content.iter().take(wrapper_tail_len))
                    .map(|item| match item {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            approx_token_count(text)
                        }
                        ContentItem::InputImage { .. } => 0,
                    })
                    .sum::<usize>();
                if wrapper_tokens <= remaining {
                    remaining = remaining.saturating_sub(wrapper_tokens);
                    truncated_content.push(content_item);
                    for _ in 0..wrapper_tail_len {
                        if let Some(item) = remaining_content.pop_front() {
                            truncated_content.push(item);
                        }
                    }
                } else {
                    for _ in 0..wrapper_tail_len {
                        let _ = remaining_content.pop_front();
                    }
                    if wrapper_has_omission {
                        return RetainedMessageTruncation::OmissionDidNotFit;
                    }
                }
                continue;
            }
        }
        match &mut content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                let is_omission = is_compacted_image_omission_text(text);
                if remaining == 0 && is_omission {
                    return RetainedMessageTruncation::OmissionDidNotFit;
                }
                if remaining == 0 {
                    continue;
                }

                let token_count = approx_token_count(text);
                if token_count <= remaining {
                    remaining = remaining.saturating_sub(token_count);
                } else if is_omission {
                    return RetainedMessageTruncation::OmissionDidNotFit;
                } else {
                    *text = truncate_text(text, TruncationPolicy::Tokens(remaining));
                    remaining = 0;
                }
                if !text.is_empty() {
                    truncated_content.push(content_item);
                }
            }
            ContentItem::InputImage { .. } => truncated_content.push(content_item),
        }
    }

    if truncated_content.is_empty() {
        return RetainedMessageTruncation::Empty;
    }

    RetainedMessageTruncation::Retained(Box::new(ResponseItem::Message {
        id,
        role,
        content: truncated_content,
        phase,
        internal_chat_message_metadata_passthrough: metadata,
    }))
}
