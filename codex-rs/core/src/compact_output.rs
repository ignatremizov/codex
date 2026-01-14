//! Local compaction output limits and human-readable checkpoint metadata.

use std::path::Path;
use std::time::Duration;

use super::CompactedUserMessage;
use super::SUMMARY_PREFIX;
use super::content_items_to_text;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

pub(super) const COMPACT_TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const COMPACT_LARGE_TURN_CHAR_THRESHOLD: usize = 2_000;
const COMPACT_LARGE_TURN_MAX: usize = 8;
const DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS: usize = 272_000;

pub(super) fn compaction_output_token_limit(model_context_window: Option<i64>) -> usize {
    let window = model_context_window
        .and_then(|window| usize::try_from(window).ok())
        .unwrap_or(DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS);
    (window / 2).max(1)
}

pub(super) fn output_tokens_for_item(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::Message { role, content, .. } if role == "assistant" => {
            content_items_to_text(content)
                .as_deref()
                .map(approx_token_count)
                .unwrap_or_default()
        }
        _ => 0,
    }
}

pub(super) fn summary_for_event(summary_text: &str) -> Option<String> {
    let text = summary_text
        .strip_prefix(SUMMARY_PREFIX)
        .and_then(|text| text.strip_prefix('\n'))
        .unwrap_or(summary_text);
    let text = text
        .split_once("\n\n[SESSION_METADATA]\n")
        .map(|(text, _)| text)
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub(super) fn build_session_metadata_block(
    session_id: &codex_protocol::ThreadId,
    rollout_path: Option<&Path>,
    user_messages: &[CompactedUserMessage],
    recent_turns_in_prompt: usize,
) -> String {
    let rollout_path = rollout_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(unavailable)".to_string());
    let turn_count = user_messages.len();
    let mut lines = vec![
        "[SESSION_METADATA]".to_string(),
        format!("session_id: {session_id}"),
        format!("rollout_path: {rollout_path}"),
        format!("user_turn_count: {turn_count}"),
        format!("recent_turns_in_prompt: {recent_turns_in_prompt}"),
    ];

    let large_turns: Vec<(usize, usize)> = user_messages
        .iter()
        .rev()
        .enumerate()
        .filter_map(|(index_from_end, message)| {
            let char_count = message.message.chars().count();
            if char_count >= COMPACT_LARGE_TURN_CHAR_THRESHOLD {
                Some((index_from_end, char_count))
            } else {
                None
            }
        })
        .take(COMPACT_LARGE_TURN_MAX)
        .collect();

    if !large_turns.is_empty() {
        lines.push(format!(
            "large_user_turn_char_counts (threshold {COMPACT_LARGE_TURN_CHAR_THRESHOLD}, newest first):"
        ));
        for (index_from_end, char_count) in large_turns {
            lines.push(format!(
                "turn_index_from_end: {index_from_end}, chars: {char_count}"
            ));
        }
    }

    lines.push("[/SESSION_METADATA]".to_string());
    lines.join("\n")
}

pub(super) fn selected_user_messages_with_limit(
    user_messages: &[CompactedUserMessage],
    max_tokens: usize,
) -> Vec<CompactedUserMessage> {
    let mut selected_messages: Vec<CompactedUserMessage> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(&message.message);
            if tokens <= remaining {
                selected_messages.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                let truncated =
                    truncate_text(&message.message, TruncationPolicy::Tokens(remaining));
                selected_messages.push(CompactedUserMessage {
                    id: message.id.clone(),
                    message: truncated,
                    internal_chat_message_metadata_passthrough: message
                        .internal_chat_message_metadata_passthrough
                        .clone(),
                    harness_metadata: message.harness_metadata.clone(),
                });
                break;
            }
        }
        selected_messages.reverse();
    }

    selected_messages
}

#[cfg(test)]
#[path = "compact_output_tests.rs"]
mod tests;
