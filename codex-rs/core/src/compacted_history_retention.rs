use std::collections::VecDeque;

use crate::context::is_compacted_image_omission_text;
use codex_context_fragments::AnnotatedContent;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

pub(crate) fn truncate_text_to_approx_token_budget(text: &str, max_tokens: usize) -> String {
    let mut content_budget = max_tokens;
    loop {
        let truncated = truncate_text(text, TruncationPolicy::Tokens(content_budget));
        let truncated_tokens = approx_token_count(&truncated);
        if truncated_tokens <= max_tokens {
            return truncated;
        }
        if content_budget == 0 {
            return String::new();
        }
        content_budget =
            content_budget.saturating_sub(truncated_tokens.saturating_sub(max_tokens).max(1));
    }
}

pub(crate) enum RetainedMessageTruncation {
    Retained(Box<ResponseItem>),
    OmissionDidNotFit,
    Empty,
}

pub(crate) fn contains_atomic_compacted_media(content: &[ContentItem]) -> bool {
    content.iter().enumerate().any(|(index, content_item)| {
        let ContentItem::InputText { text } = content_item else {
            return false;
        };
        if is_compacted_image_omission_text(text) {
            return true;
        }
        if !is_local_image_open_tag_with_path_text(text) {
            return false;
        }
        matches!(
            content.get(index.saturating_add(1)),
            Some(ContentItem::InputText { text }) if is_local_image_close_tag_text(text)
        ) || matches!(
            (
                content.get(index.saturating_add(1)),
                content.get(index.saturating_add(2))
            ),
            (
                Some(ContentItem::InputText { .. }),
                Some(ContentItem::InputText { text })
            ) if is_local_image_close_tag_text(text)
        )
    })
}

pub(crate) fn truncate_retained_message_to_token_budget(
    mut item: ResponseItem,
    max_tokens: usize,
) -> RetainedMessageTruncation {
    if !matches!(item, ResponseItem::Message { .. }) {
        return RetainedMessageTruncation::Retained(Box::new(item));
    }
    let Some(content) = to_annotated_content(&mut item) else {
        return RetainedMessageTruncation::Retained(Box::new(item));
    };

    let mut remaining_content = VecDeque::from(content);
    let mut remaining = max_tokens;
    let mut truncated_content = Vec::with_capacity(remaining_content.len());
    while let Some(mut content_item) = remaining_content.pop_front() {
        if remaining == 0 {
            if std::iter::once(&content_item)
                .chain(remaining_content.iter())
                .any(|item| {
                    matches!(
                        item.content(),
                        ContentItem::InputText { text }
                            if is_compacted_image_omission_text(text)
                    )
                })
            {
                return RetainedMessageTruncation::OmissionDidNotFit;
            }
            break;
        }
        if matches!(
            content_item.content(),
            ContentItem::InputText { text }
                if is_local_image_open_tag_with_path_text(text)
        ) {
            let (wrapper_tail_len, wrapper_has_omission) = match (
                remaining_content.front().map(AnnotatedContent::content),
                remaining_content.get(1).map(AnnotatedContent::content),
            ) {
                (Some(ContentItem::InputText { text }), _)
                    if is_local_image_close_tag_text(text) =>
                {
                    (1usize, false)
                }
                (
                    Some(ContentItem::InputText { text: placeholder }),
                    Some(ContentItem::InputText { text: close }),
                ) if is_local_image_close_tag_text(close) => {
                    (2usize, is_compacted_image_omission_text(placeholder))
                }
                _ => (0usize, false),
            };
            if wrapper_tail_len > 0 {
                let wrapper_tokens = std::iter::once(&content_item)
                    .chain(remaining_content.iter().take(wrapper_tail_len))
                    .map(|item| match item.content() {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            approx_token_count(text)
                        }
                        ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => 0,
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
        match content_item.content_mut() {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                let is_omission = is_compacted_image_omission_text(text);
                let token_count = approx_token_count(text);
                if token_count <= remaining {
                    remaining = remaining.saturating_sub(token_count);
                } else if is_omission {
                    return RetainedMessageTruncation::OmissionDidNotFit;
                } else {
                    *text = truncate_text_to_approx_token_budget(text, remaining);
                    remaining = 0;
                }
                if !text.is_empty() {
                    truncated_content.push(content_item);
                }
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                truncated_content.push(content_item);
            }
        }
    }

    if truncated_content.is_empty() {
        return RetainedMessageTruncation::Empty;
    }

    let _ = set_annotated_content(&mut item, truncated_content);
    RetainedMessageTruncation::Retained(Box::new(item))
}

#[cfg(test)]
#[path = "compacted_history_retention_tests.rs"]
mod tests;
