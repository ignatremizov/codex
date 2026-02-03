use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::ModelProviderInfo;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
#[cfg(test)]
use crate::codex::PreviousTurnSettings;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::codex::get_last_assistant_message_from_turn;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::event_mapping::parse_turn_item;
use crate::protocol::CompactedItem;
use crate::protocol::ContextCompactedEvent;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::protocol::WarningEvent;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::truncate_text;
use crate::util::backoff;
use codex_features::Feature;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use futures::prelude::*;
use tokio::time::timeout;
use tracing::error;

pub const SUMMARIZATION_PROMPT: &str = include_str!("../templates/compact/prompt.md");
pub const SUMMARY_PREFIX: &str = include_str!("../templates/compact/summary_prefix.md");
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;
pub(crate) const COMPACT_TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const COMPACT_LARGE_TURN_CHAR_THRESHOLD: usize = 2_000;
const COMPACT_LARGE_TURN_MAX: usize = 8;
const DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS: usize = 272_000;

/// Controls whether compaction replacement history must include initial context.
///
/// Pre-turn/manual compaction variants use `DoNotInject`: they replace history with a summary and
/// clear `reference_context_item`, so the next regular turn will fully reinject initial context
/// after compaction.
///
/// Mid-turn compaction must use `BeforeLastUserMessage` because the model is trained to see the
/// compaction summary as the last item in history after mid-turn compaction; we therefore inject
/// initial context into the replacement history just above the last real user message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitialContextInjection {
    BeforeLastUserMessage,
    DoNotInject,
}

pub(crate) fn should_use_remote_compact_task(
    session: &Session,
    provider: &ModelProviderInfo,
) -> bool {
    provider.is_openai() && session.enabled(Feature::RemoteCompaction)
}

pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let prompt = turn_context.compact_prompt().to_string();
    let input = vec![UserInput::Text {
        text: prompt,
        // Compaction prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];

    run_compact_task_inner(sess, turn_context, input, initial_context_injection).await?;
    Ok(())
}

pub(crate) fn compaction_output_token_limit(turn_context: &TurnContext) -> usize {
    compaction_output_token_limit_for_context_window(turn_context.model_context_window())
}

fn compaction_output_token_limit_for_context_window(model_context_window: Option<i64>) -> usize {
    let model_context_window = model_context_window
        .and_then(|window| usize::try_from(window).ok())
        .unwrap_or(DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS);
    compaction_output_token_limit_for_window(model_context_window)
}

fn compaction_output_token_limit_for_window(model_context_window: usize) -> usize {
    let cap = model_context_window / 2;
    cap.max(1)
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;
    run_compact_task_inner(
        sess.clone(),
        turn_context,
        input,
        InitialContextInjection::DoNotInject,
    )
    .await
}

async fn run_compact_task_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let compaction_item = ContextCompactionItem::new();
    let started_compaction_item = TurnItem::ContextCompaction(compaction_item.clone());
    sess.emit_turn_item_started(&turn_context, &started_compaction_item)
        .await;
    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);

    let mut history = sess.clone_history().await;
    history.record_items(
        &[initial_input_for_turn.into()],
        turn_context.truncation_policy,
    );

    let mut truncated_count = 0usize;

    let max_retries = turn_context.provider.stream_max_retries();
    let mut retries = 0;
    let mut client_session = sess.services.model_client.new_session();
    // Reuse one client session so turn-scoped state (sticky routing, websocket incremental
    // request tracking)
    // survives retries within this compact turn.

    loop {
        // Clone is required because of the loop
        let turn_input = history
            .clone()
            .for_prompt(&turn_context.model_info.input_modalities);
        let turn_input_len = turn_input.len();
        let prompt = Prompt {
            input: turn_input,
            base_instructions: sess.get_base_instructions().await,
            personality: turn_context.personality,
            ..Default::default()
        };
        let turn_metadata_header = turn_context.turn_metadata_state.current_header_value();
        let output_token_limit = compaction_output_token_limit(turn_context.as_ref());
        let attempt_result = drain_to_completed(
            &sess,
            turn_context.as_ref(),
            &mut client_session,
            turn_metadata_header.as_deref(),
            &prompt,
            output_token_limit,
        )
        .await;

        match attempt_result {
            Ok(()) => {
                if truncated_count > 0 {
                    sess.notify_background_event(
                        turn_context.as_ref(),
                        format!(
                            "Trimmed {truncated_count} older thread item(s) before compacting so the prompt fits the model context window."
                        ),
                    )
                    .await;
                }
                break;
            }
            Err(CodexErr::Interrupted) => {
                return Err(CodexErr::Interrupted);
            }
            Err(
                e @ (CodexErr::CompactionTimedOut { .. } | CodexErr::CompactionOutputLimit { .. }),
            ) => {
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(&turn_context, event).await;
                return Err(e);
            }
            Err(e @ CodexErr::ContextWindowExceeded) => {
                if turn_input_len > 1 {
                    // Trim from the beginning to preserve cache (prefix-based) and keep recent messages intact.
                    error!(
                        "Context window exceeded while compacting; removing oldest history item. Error: {e}"
                    );
                    history.remove_first_item();
                    truncated_count += 1;
                    retries = 0;
                    continue;
                }
                sess.set_total_tokens_full(turn_context.as_ref()).await;
                let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                sess.send_event(&turn_context, event).await;
                return Err(e);
            }
            Err(e) => {
                if retries < max_retries {
                    retries += 1;
                    let delay = backoff(retries);
                    sess.notify_stream_error(
                        turn_context.as_ref(),
                        format!("Reconnecting... {retries}/{max_retries}"),
                        e,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    continue;
                } else {
                    let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                    sess.send_event(&turn_context, event).await;
                    return Err(e);
                }
            }
        }
    }

    let history_snapshot = sess.clone_history().await;
    let history_items = history_snapshot.raw_items();
    let summary_suffix = get_last_assistant_message_from_turn(history_items).unwrap_or_default();
    let user_messages = collect_user_messages(history_items);
    let rollout_path = {
        let rollout_guard = sess.services.rollout.lock().await;
        rollout_guard
            .as_ref()
            .map(|recorder| recorder.rollout_path.clone())
    };
    let session_metadata = build_session_metadata_block(
        &sess.conversation_id,
        rollout_path.as_deref(),
        &user_messages,
    );
    let summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}\n\n{session_metadata}");
    let summary_for_event_text = summary_for_event(&summary_text);

    let mut new_history = build_compacted_history(Vec::new(), &user_messages, &summary_text);

    if matches!(
        initial_context_injection,
        InitialContextInjection::BeforeLastUserMessage
    ) {
        let initial_context = sess.build_initial_context(turn_context.as_ref()).await;
        new_history =
            insert_initial_context_before_last_real_user_or_summary(new_history, initial_context);
    }
    let ghost_snapshots: Vec<ResponseItem> = history_items
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();
    new_history.extend(ghost_snapshots);
    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => Some(turn_context.to_turn_context_item()),
    };
    let compacted_item = CompactedItem {
        message: summary_text.clone(),
        replacement_history: Some(new_history.clone()),
    };
    sess.replace_compacted_history(new_history, reference_context_item, compacted_item)
        .await;
    sess.recompute_token_usage(&turn_context).await;

    let mut completed_compaction_item = compaction_item;
    completed_compaction_item.summary = summary_for_event_text.clone();
    completed_compaction_item.message = Some(summary_text.clone());

    sess.emit_turn_item_completed(
        &turn_context,
        TurnItem::ContextCompaction(completed_compaction_item),
    )
    .await;
    let event = EventMsg::ContextCompacted(ContextCompactedEvent {
        summary: summary_for_event_text,
        message: Some(summary_text),
    });
    sess.send_event(&turn_context, event).await;
    let warning = EventMsg::Warning(WarningEvent {
        message: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_string(),
    });
    sess.send_event(&turn_context, warning).await;
    Ok(())
}

pub fn content_items_to_text(content: &[ContentItem]) -> Option<String> {
    let mut pieces = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.is_empty() {
                    pieces.push(text.as_str());
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join("\n"))
    }
}

pub(crate) fn collect_user_messages(items: &[ResponseItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match crate::event_mapping::parse_turn_item(item) {
            Some(TurnItem::UserMessage(user)) => {
                if is_summary_message(&user.message()) {
                    None
                } else {
                    Some(user.message())
                }
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn extract_compacted_summary_text(items: &[ResponseItem]) -> Option<String> {
    let mut fallback_user_message = None;
    for item in items.iter().rev() {
        let Some(TurnItem::UserMessage(user)) = parse_turn_item(item) else {
            continue;
        };
        let message = user.message();
        if message.trim().is_empty() || is_environment_context_message(&message) {
            continue;
        }
        if is_summary_message(&message) {
            return Some(message);
        }
        if fallback_user_message.is_none() {
            fallback_user_message = Some(message);
        }
    }
    if fallback_user_message.is_some() {
        return fallback_user_message;
    }

    items.iter().rev().find_map(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "assistant" => {
            content_items_to_text(content).and_then(|text| {
                if text.trim().is_empty() || is_environment_context_message(&text) {
                    None
                } else {
                    Some(text)
                }
            })
        }
        _ => None,
    })
}

pub(crate) fn summary_for_event(summary_text: &str) -> Option<String> {
    let summary_text = summary_text
        .strip_prefix(SUMMARY_PREFIX)
        .and_then(|text| text.strip_prefix('\n'))
        .unwrap_or(summary_text)
        .trim();

    if summary_text.is_empty() {
        None
    } else {
        Some(summary_text.to_string())
    }
}
pub(crate) fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

/// Inserts canonical initial context into compacted replacement history at the
/// model-expected boundary.
///
/// Placement rules:
/// - Prefer immediately before the last real user message.
/// - If no real user messages remain, insert before the compaction summary so
///   the summary stays last.
/// - If there are no user messages, insert before the last compaction item so
///   that item remains last (remote compaction may return only compaction items).
/// - If there are no user messages or compaction items, append the context.
pub(crate) fn insert_initial_context_before_last_real_user_or_summary(
    mut compacted_history: Vec<ResponseItem>,
    initial_context: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    let mut last_user_or_summary_index = None;
    let mut last_real_user_index = None;
    for (i, item) in compacted_history.iter().enumerate().rev() {
        let Some(TurnItem::UserMessage(user)) = crate::event_mapping::parse_turn_item(item) else {
            continue;
        };
        // Compaction summaries are encoded as user messages, so track both:
        // the last real user message (preferred insertion point) and the last
        // user-message-like item (fallback summary insertion point).
        last_user_or_summary_index.get_or_insert(i);
        if !is_summary_message(&user.message()) {
            last_real_user_index = Some(i);
            break;
        }
    }
    let last_compaction_index = compacted_history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, item)| matches!(item, ResponseItem::Compaction { .. }).then_some(i));
    let insertion_index = last_real_user_index
        .or(last_user_or_summary_index)
        .or(last_compaction_index);

    // Re-inject canonical context from the current session since we stripped it
    // from the pre-compaction history. Prefer placing it before the last real
    // user message; if there is no real user message left, place it before the
    // summary or compaction item so the compaction item remains last.
    if let Some(insertion_index) = insertion_index {
        compacted_history.splice(insertion_index..insertion_index, initial_context);
    } else {
        compacted_history.extend(initial_context);
    }

    compacted_history
}

fn is_environment_context_message(message: &str) -> bool {
    let trimmed = message.trim_start();
    trimmed
        .to_ascii_lowercase()
        .starts_with("<environment_context>")
}
pub(crate) fn build_compacted_history(
    initial_context: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
) -> Vec<ResponseItem> {
    build_compacted_history_with_limit(
        initial_context,
        user_messages,
        summary_text,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    )
}

fn build_compacted_history_with_limit(
    mut history: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<ResponseItem> {
    let mut selected_messages: Vec<String> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(message);
            if tokens <= remaining {
                selected_messages.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                let truncated = truncate_text(message, TruncationPolicy::Tokens(remaining));
                selected_messages.push(truncated);
                break;
            }
        }
        selected_messages.reverse();
    }

    for message in &selected_messages {
        history.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: message.clone(),
            }],
            end_turn: None,
            phase: None,
        });
    }

    let summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };

    history.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text: summary_text }],
        end_turn: None,
        phase: None,
    });

    history
}

fn build_session_metadata_block(
    session_id: &codex_protocol::ThreadId,
    rollout_path: Option<&Path>,
    user_messages: &[String],
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
        format!("recent_turns_in_prompt: {turn_count}"),
    ];

    let large_turns: Vec<(usize, usize)> = user_messages
        .iter()
        .rev()
        .enumerate()
        .filter_map(|(index_from_end, message)| {
            let char_count = message.chars().count();
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

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    prompt: &Prompt,
    output_token_limit: usize,
) -> CodexResult<()> {
    let mut stream = client_session
        .stream(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            turn_context.reasoning_effort,
            turn_context.reasoning_summary,
            turn_context.config.service_tier,
            turn_metadata_header,
        )
        .await?;
    let mut output_tokens = 0usize;
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= COMPACT_TURN_TIMEOUT {
            return Err(CodexErr::CompactionTimedOut {
                limit: COMPACT_TURN_TIMEOUT,
            });
        }
        let remaining = COMPACT_TURN_TIMEOUT.saturating_sub(elapsed);
        let maybe_event = timeout(remaining, stream.next()).await;
        let maybe_event = match maybe_event {
            Ok(event) => event,
            Err(_) => {
                return Err(CodexErr::CompactionTimedOut {
                    limit: COMPACT_TURN_TIMEOUT,
                });
            }
        };
        let Some(event) = maybe_event else {
            return Err(CodexErr::Stream(
                "stream closed before response.completed".into(),
                None,
            ));
        };
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                output_tokens = output_tokens.saturating_add(output_tokens_for_item(&item));
                if output_tokens > output_token_limit {
                    return Err(CodexErr::CompactionOutputLimit {
                        max_tokens: output_token_limit,
                        actual_tokens: output_tokens,
                    });
                }
                sess.record_into_history(std::slice::from_ref(&item), turn_context)
                    .await;
            }
            Ok(ResponseEvent::ServerReasoningIncluded(included)) => {
                sess.set_server_reasoning_included(included).await;
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                sess.update_token_usage_info(turn_context, token_usage.as_ref())
                    .await;
                return Ok(());
            }
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

fn output_tokens_for_item(item: &ResponseItem) -> usize {
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

pub(crate) fn assistant_output_tokens_for_items(items: &[ResponseItem]) -> usize {
    items.iter().map(output_tokens_for_item).sum()
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
