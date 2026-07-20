use std::collections::VecDeque;
use std::sync::Arc;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::compaction_status_from_result;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote::process_compacted_history;
use crate::compact_remote::should_keep_compacted_history_item;
use crate::context::CompactedMediaSanitization;
use crate::context::expire_compacted_media_references;
use crate::context::is_compacted_image_omission_text;
use crate::context::sanitize_compacted_media;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::CONTEXT_COMPACTION_DECODING_MESSAGE;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TruncationPolicy;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout_trace::CompactionCheckpointTracePayload;
use codex_rollout_trace::InferenceTraceContext;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

// Mirror the current /responses/compact retained-message default while the
// server-side path remains the reference implementation.
const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 128_000;
// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;

pub(crate) struct InlineRemoteAutoCompactTask<'a> {
    pub(crate) sess: Arc<Session>,
    pub(crate) step_context: Arc<StepContext>,
    pub(crate) fallback_step_context: Option<Arc<StepContext>>,
    pub(crate) client_session: &'a mut ModelClientSession,
    pub(crate) initial_context_injection: InitialContextInjection,
    pub(crate) reason: CompactionReason,
    pub(crate) phase: CompactionPhase,
    pub(crate) cancellation_token: &'a CancellationToken,
}

pub(crate) async fn run_inline_remote_auto_compact_task(
    task: InlineRemoteAutoCompactTask<'_>,
) -> CodexResult<()> {
    let InlineRemoteAutoCompactTask {
        sess,
        step_context,
        fallback_step_context,
        client_session,
        initial_context_injection,
        reason,
        phase,
        cancellation_token,
    } = task;
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        reason,
        CompactionImplementation::ResponsesCompactionV2,
        phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        fallback_step_context.as_ref(),
        Some(client_session),
        initial_context_injection,
        compaction_metadata,
        cancellation_token,
    )
    .await
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    // Standalone compaction is its own request boundary, so it captures a fresh step.
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompactionV2,
        CompactionPhase::StandaloneTurn,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*client_session*/ None,
        InitialContextInjection::DoNotInject,
        compaction_metadata,
        cancellation_token,
    )
    .await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let trigger = compaction_metadata.trigger();
    let reason = compaction_metadata.reason();
    let implementation = compaction_metadata.implementation();
    let phase = compaction_metadata.phase();
    let mut analytics_details = CompactionAnalyticsDetails {
        active_context_tokens_before: Some(sess.get_total_token_usage().await),
        ..Default::default()
    };
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        implementation,
        phase,
    )
    .await;
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => {
            let error = CodexErr::TurnAborted;
            attempt
                .track(
                    sess.as_ref(),
                    codex_analytics::CompactionStatus::Interrupted,
                    Some(&error),
                    analytics_details,
                )
                .await;
            return Err(error);
        }
    }
    let result = run_remote_compact_task_inner_impl(
        RemoteCompactTask {
            sess,
            step_context,
            fallback_step_context,
            client_session,
            initial_context_injection,
            compaction_metadata,
            cancellation_token,
        },
        &mut analytics_details,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if result.is_ok() {
        let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
        if let PostCompactHookOutcome::Stopped = post_compact_outcome {
            attempt
                .track(sess.as_ref(), status, codex_error, analytics_details)
                .await;
            return Err(CodexErr::TurnAborted);
        }
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    match result {
        Ok(()) => Ok(()),
        Err(err @ CodexErr::TurnAborted) => Err(err),
        Err(err) => {
            sess.track_turn_codex_error(turn_context, &err);
            let event = EventMsg::Error(
                err.to_error_event(Some("Error running remote compact task".to_string())),
            );
            sess.send_event(turn_context, event).await;
            Err(err)
        }
    }
}

struct RemoteCompactTask<'a> {
    sess: &'a Arc<Session>,
    step_context: &'a Arc<StepContext>,
    fallback_step_context: Option<&'a Arc<StepContext>>,
    client_session: Option<&'a mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    cancellation_token: &'a CancellationToken,
}

async fn run_remote_compact_task_inner_impl(
    task: RemoteCompactTask<'_>,
    analytics_details: &mut CompactionAnalyticsDetails,
) -> CodexResult<()> {
    let RemoteCompactTask {
        sess,
        step_context,
        fallback_step_context,
        mut client_session,
        initial_context_injection,
        compaction_metadata,
        cancellation_token,
    } = task;
    let turn_context = &step_context.turn;
    let mut context_compaction_item = ContextCompactionItem::new();
    let compaction_id = context_compaction_item.id.clone();
    let compaction_trace = sess.services.rollout_thread_trace.compaction_trace_context(
        turn_context.sub_id.as_str(),
        compaction_id.as_str(),
        turn_context.model_info.slug.as_str(),
        turn_context.provider.info().name.as_str(),
    );
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item.clone());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;

    let attempt = run_remote_compact_v2_attempt(
        sess,
        step_context,
        client_session.as_deref_mut(),
        &compaction_trace,
        compaction_metadata,
        analytics_details,
    )
    .await;
    let (attempt, compaction_turn_context) = match attempt {
        Ok(attempt) => (attempt, turn_context),
        Err(error) => {
            let Some(fallback_step_context) = fallback_step_context else {
                return Err(error);
            };
            if !should_retry_with_current_model(&error) {
                return Err(error);
            }
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace =
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    compaction_id.as_str(),
                    fallback_turn_context.model_info.slug.as_str(),
                    fallback_turn_context.provider.info().name.as_str(),
                );
            let fallback_result = run_remote_compact_v2_attempt(
                sess,
                fallback_step_context,
                client_session,
                &fallback_compaction_trace,
                compaction_metadata,
                analytics_details,
            )
            .await;
            record_model_fallback(
                &sess.services.session_telemetry,
                turn_context.model_info.slug.as_str(),
                fallback_turn_context.model_info.slug.as_str(),
                compaction_metadata.reason(),
                compaction_metadata.implementation(),
                fallback_result.as_ref().err(),
            );
            match fallback_result {
                Ok(attempt) => (attempt, fallback_turn_context),
                Err(_) => return Err(error),
            }
        }
    };
    let RemoteCompactV2Attempt {
        trace_input_history,
        compacted_prefix_len,
        prompt_input,
        compaction_output,
        token_usage,
        owned_client_session: _owned_client_session,
    } = attempt;
    let compaction_summary_tokens = token_usage.as_ref().map(|usage| usage.output_tokens);
    if let Some(token_usage) = token_usage {
        info!(
            turn_id = %turn_context.sub_id,
            compaction_summary_tokens = token_usage.output_tokens,
            active_context_tokens_before = token_usage.input_tokens,
            cached_input_tokens = token_usage.cached_input_tokens,
            "remote compaction v2 token usage"
        );
        sess.record_rollout_budget_usage(&token_usage)?;
        analytics_details.active_context_tokens_before = Some(token_usage.input_tokens);
        analytics_details.compaction_summary_tokens = Some(token_usage.output_tokens);
        analytics_details.cached_input_tokens = Some(token_usage.cached_input_tokens);
        analytics_details.cache_write_input_tokens = Some(token_usage.cache_write_input_tokens);
    }
    let (compacted_history, media_sanitization) = build_v2_compacted_history(
        &trace_input_history,
        compacted_prefix_len,
        compaction_output,
    );
    let explicit_mcp_context = crate::compact::collect_mcp_server_use_context_items(&prompt_input);
    analytics_details.retained_image_count = Some(0);
    analytics_details.omitted_image_count = Some(
        analytics_details
            .omitted_image_count
            .unwrap_or_default()
            .saturating_add(media_sanitization.omitted_image_count),
    );
    analytics_details.omitted_inline_media_bytes = Some(
        analytics_details
            .omitted_inline_media_bytes
            .unwrap_or_default()
            .saturating_add(media_sanitization.omitted_inline_media_bytes),
    );
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (mut new_history, world_state_baseline) = process_compacted_history(
        sess.as_ref(),
        compaction_turn_context.as_ref(),
        compacted_history,
        &initial_context_injection,
    )
    .await;
    new_history = crate::compact::insert_mcp_server_use_context_items_at_compaction_boundary(
        new_history,
        explicit_mcp_context,
    );

    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: Some(new_history.clone()),
        compaction_summary_tokens,
        window_number: Some(new_window_number),
        first_window_id: Some(new_window_ids.first_window_id.to_string()),
        previous_window_id: new_window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(new_window_ids.window_id.to_string()),
        replacement_history_media_sanitized_prefix_len: None,
        replacement_history_media_repair: false,
    };
    let final_history = sess
        .replace_compacted_history(
            Arc::clone(compaction_turn_context),
            new_history,
            reference_context_item,
            world_state_baseline,
            compacted_item,
        )
        .await?;
    compaction_trace.record_installed(&CompactionCheckpointTracePayload {
        input_history: &trace_input_history,
        replacement_history: &final_history,
    });
    sess.recompute_token_usage(compaction_turn_context).await;
    if crate::compact_handoff_summary::should_decode_remote_compaction_handoff(
        compaction_turn_context.config.as_ref(),
    ) {
        sess.emit_transient_context_compaction_status(
            compaction_turn_context,
            context_compaction_item.id.clone(),
            CONTEXT_COMPACTION_DECODING_MESSAGE.to_string(),
        )
        .await;
    }
    context_compaction_item.message =
        crate::compact_handoff_summary::summarize_remote_compaction_handoff(
            sess,
            compaction_turn_context,
            &final_history,
            cancellation_token,
        )
        .await;

    sess.emit_turn_item_completed(
        compaction_turn_context,
        TurnItem::ContextCompaction(context_compaction_item),
    )
    .await;
    Ok(())
}

struct RemoteCompactionV2Output {
    compaction_output: ResponseItem,
    response_id: String,
    token_usage: Option<TokenUsage>,
}

async fn run_remote_compaction_request_v2(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
) -> CodexResult<RemoteCompactionV2Output> {
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retries = 0;
    loop {
        let result = match client_session
            .stream(
                prompt,
                &turn_context.model_info,
                &turn_context.session_telemetry,
                turn_context.reasoning_effort.clone(),
                turn_context.reasoning_summary,
                turn_context.config.service_tier.clone(),
                responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
        {
            Ok(stream) => collect_compaction_output(stream).await,
            Err(err) => Err(err),
        };

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                handle_retryable_response_stream_error(
                    &mut retries,
                    max_retries,
                    err,
                    client_session,
                    sess,
                    turn_context,
                    ResponsesStreamRequest::RemoteCompactionV2,
                )
                .await?;
            }
        }
    }
}

async fn collect_compaction_output(
    mut stream: ResponseStream,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut compaction_output = None;
    let mut saw_completed = false;
    let mut completed_response_id = None;
    let mut completed_token_usage = None;
    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(item) => {
                output_item_count += 1;
                if let ResponseItem::Compaction { .. } = item {
                    compaction_count += 1;
                    if compaction_output.is_none() {
                        compaction_output = Some(item);
                    }
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                saw_completed = true;
                completed_response_id = Some(response_id);
                completed_token_usage = token_usage;
                break;
            }
            _ => {}
        }
    }

    if !saw_completed {
        return Err(CodexErr::Stream(
            "remote compaction v2 stream closed before response.completed".to_string(),
            None,
        ));
    }

    if compaction_count != 1 {
        return Err(CodexErr::Fatal(format!(
            "remote compaction v2 expected exactly one compaction output item, got {compaction_count} from {output_item_count} output items"
        )));
    }

    let Some(compaction_output) = compaction_output else {
        unreachable!("compaction output must exist when count is exactly one");
    };
    let Some(response_id) = completed_response_id else {
        unreachable!("response id must exist after response.completed");
    };
    Ok(RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage: completed_token_usage,
    })
}

fn build_v2_compacted_history(
    replacement_history_input: &[ResponseItem],
    compacted_prefix_len: usize,
    compaction_output: ResponseItem,
) -> (Vec<ResponseItem>, CompactedMediaSanitization) {
    let compacted_prefix_len = compacted_prefix_len.min(replacement_history_input.len());
    // Partition only the cloned replacement-history candidates. The compaction request has
    // already seen inherited paths, while the next checkpoint should keep references introduced
    // in the current window and let older paths age out like other compacted context.
    let mut inherited = replacement_history_input[..compacted_prefix_len].to_vec();
    let mut current = replacement_history_input[compacted_prefix_len..].to_vec();
    let inherited_retention = inherited
        .iter()
        .map(|item| {
            is_retained_for_remote_compaction_v2(item) && should_keep_compacted_history_item(item)
        })
        .collect::<Vec<_>>();
    let current_retention = current
        .iter()
        .map(|item| {
            is_retained_for_remote_compaction_v2(item) && should_keep_compacted_history_item(item)
        })
        .collect::<Vec<_>>();

    let mut media_sanitization = sanitize_compacted_media(&mut inherited);
    expire_compacted_media_references(&mut inherited);
    media_sanitization.accumulate(sanitize_compacted_media(&mut current));
    // Sanitization can place the checkpoint-wide omission marker in a tool output because that was
    // the latest media-bearing item. Remote-v2 retention drops tool outputs, so capture the marker
    // before filtering and rehome it into a retained current-window message below.
    let current_omission_text = current.iter().find_map(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().find_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text }
                if is_compacted_image_omission_text(text) =>
            {
                Some(text.clone())
            }
            ContentItem::InputText { .. }
            | ContentItem::OutputText { .. }
            | ContentItem::InputImage { .. } => None,
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().and_then(|content| {
                content.iter().find_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text }
                        if is_compacted_image_omission_text(text) =>
                    {
                        Some(text.clone())
                    }
                    FunctionCallOutputContentItem::InputText { .. }
                    | FunctionCallOutputContentItem::InputImage { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
            })
        }
        _ => None,
    });
    inherited = inherited
        .into_iter()
        .zip(inherited_retention)
        .filter_map(|(item, retain)| {
            (retain
                && !matches!(
                    &item,
                    ResponseItem::Message { content, .. } if content.is_empty()
                ))
            .then_some(item)
        })
        .collect();
    current = current
        .into_iter()
        .zip(current_retention)
        .filter_map(|(item, retain)| retain.then_some(item))
        .collect();
    let current_has_omission = current.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { content, .. }
                if content.iter().any(|item| {
                    matches!(
                        item,
                        ContentItem::InputText { text } | ContentItem::OutputText { text }
                            if is_compacted_image_omission_text(text)
                    )
                })
        )
    });
    if !current_has_omission && let Some(omission_text) = current_omission_text {
        if let Some(ResponseItem::Message { content, .. }) = current.last_mut() {
            content.push(ContentItem::InputText {
                text: omission_text,
            });
        } else {
            current.push(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: omission_text,
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            });
        }
    }
    inherited.extend(current);
    let mut retained =
        truncate_retained_messages_for_remote_compaction(inherited, RETAINED_MESSAGE_TOKEN_BUDGET);
    retained.push(compaction_output);
    (retained, media_sanitization)
}

fn is_retained_for_remote_compaction_v2(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, .. } = item else {
        return false;
    };

    matches!(role.as_str(), "user" | "developer" | "system")
}

fn truncate_retained_messages_for_remote_compaction(
    items: Vec<ResponseItem>,
    max_tokens: usize,
) -> Vec<ResponseItem> {
    let mut remaining = max_tokens;
    let mut truncated_reversed = Vec::with_capacity(items.len());
    for item in items.into_iter().rev() {
        if remaining == 0 {
            continue;
        }

        let token_count = message_text_token_count(&item).max(1);
        if token_count <= remaining {
            truncated_reversed.push(item);
            remaining = remaining.saturating_sub(token_count);
        } else {
            match truncate_message_text_to_token_budget(item, /*max_tokens*/ remaining) {
                RetainedMessageTruncation::Retained(truncated_item) => {
                    truncated_reversed.push(*truncated_item);
                    remaining = 0;
                }
                RetainedMessageTruncation::OmissionDidNotFit => remaining = 0,
                RetainedMessageTruncation::Empty => {}
            }
        }
    }
    truncated_reversed.reverse();
    truncated_reversed
}

fn message_text_token_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return 0;
    };

    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                approx_token_count(text)
            }
            ContentItem::InputImage { .. } => 0,
        })
        .sum()
}

enum RetainedMessageTruncation {
    Retained(Box<ResponseItem>),
    OmissionDidNotFit,
    Empty,
}

fn truncate_message_text_to_token_budget(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CompactedImageOmission;
    use crate::context::ContextualUserFragment;
    use crate::context::sanitize_compacted_media_before_latest_compaction;
    use crate::context::sanitize_compacted_media_prefix;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::MessagePhase;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn message(role: &str, text: &str, phase: Option<MessagePhase>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn response_stream(events: Vec<CodexResult<ResponseEvent>>) -> ResponseStream {
        let (tx_event, rx_event) = mpsc::channel(events.len().max(1));
        for event in events {
            tx_event
                .try_send(event)
                .expect("response stream test channel should have capacity");
        }
        drop(tx_event);
        ResponseStream {
            rx_event,
            consumer_dropped: CancellationToken::new(),
        }
    }

    #[test]
    fn build_v2_compacted_history_filters_to_installed_retention_shape() {
        let input = vec![
            message("developer", "dev", /*phase*/ None),
            message("system", "sys", /*phase*/ None),
            message("user", "user", /*phase*/ None),
            message("assistant", "commentary", Some(MessagePhase::Commentary)),
            message("assistant", "final", Some(MessagePhase::FinalAnswer)),
            ResponseItem::FunctionCall {
                id: None,
                name: "shell_command".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call_1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "old".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 0, output.clone());

        assert_eq!(
            history,
            vec![message("user", "user", /*phase*/ None), output]
        );
    }

    #[test]
    fn build_v2_compacted_history_discards_messages_before_truncating() {
        let old = message("user", "old", /*phase*/ None);
        let new = message("user", "new", /*phase*/ None);
        let huge_developer_message = "d".repeat((RETAINED_MESSAGE_TOKEN_BUDGET + 1) * 4);
        let huge_contextual_message = format!(
            "<environment_context>\n{}\n</environment_context>",
            "c".repeat((RETAINED_MESSAGE_TOKEN_BUDGET + 1) * 4)
        );
        let input = vec![
            old.clone(),
            message("developer", &huge_developer_message, /*phase*/ None),
            message("user", &huge_contextual_message, /*phase*/ None),
            new.clone(),
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 0, output.clone());

        assert_eq!(history, vec![old, new, output]);
    }

    #[test]
    fn build_v2_compacted_history_sanitizes_retained_input_images() {
        let input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "user".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,def".to_string(),
                    detail: None,
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 0, output);

        assert_eq!(sanitization.omitted_image_count, 2);
        assert_eq!(sanitization.omitted_inline_media_bytes, 50);
        assert!(
            history.iter().all(|item| {
                !matches!(
                    item,
                    ResponseItem::Message { content, .. }
                        if content
                            .iter()
                            .any(|item| matches!(item, ContentItem::InputImage { .. }))
                )
            }),
            "compacted history must not retain inline image payloads"
        );
    }

    #[test]
    fn build_v2_compacted_history_rehomes_omission_from_filtered_tool_output() {
        let omission = CompactedImageOmission::unavailable().render();
        let mut input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "user".to_string(),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,user".to_string(),
                        detail: None,
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "tool-call".to_string(),
                output: FunctionCallOutputPayload::from_content_items(vec![
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,tool".to_string(),
                        detail: None,
                    },
                ]),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        sanitize_compacted_media(&mut input);
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 0, output.clone());

        assert_eq!(
            history,
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: "user".to_string(),
                        },
                        ContentItem::InputText { text: omission },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                output,
            ]
        );
    }

    #[test]
    fn build_v2_compacted_history_retains_tool_only_current_window_omission() {
        let input = vec![ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "tool-call".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,tool".to_string(),
                    detail: None,
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        }];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 0, output.clone());

        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            history,
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: CompactedImageOmission::unavailable().render(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                output,
            ]
        );
    }

    #[test]
    fn build_v2_compacted_history_keeps_only_current_window_image_paths() {
        let old_path = "/tmp/old-window.png";
        let current_path = "/tmp/current-window.png";
        let mut input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #1] path=\"{old_path}\">"),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,old".to_string(),
                        detail: None,
                    },
                    ContentItem::InputText {
                        text: "</image>".to_string(),
                    },
                    ContentItem::InputText {
                        text: "old image context".to_string(),
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "previous summary".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #2] path=\"{current_path}\">"),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,current".to_string(),
                        detail: None,
                    },
                    ContentItem::InputText {
                        text: "</image>".to_string(),
                    },
                    ContentItem::InputText {
                        text: "current image context".to_string(),
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let pre_compaction_sanitization =
            sanitize_compacted_media_before_latest_compaction(&mut input);
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 2, output.clone());

        assert_eq!(pre_compaction_sanitization.omitted_image_count, 1);
        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            history,
            vec![
                message("user", "old image context", /*phase*/ None),
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: format!("<image name=[Image #2] path=\"{current_path}\">"),
                        },
                        ContentItem::InputText {
                            text: CompactedImageOmission::reopenable_local_image().render(),
                        },
                        ContentItem::InputText {
                            text: "</image>".to_string(),
                        },
                        ContentItem::InputText {
                            text: "current image context".to_string(),
                        },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                output,
            ]
        );
    }

    #[test]
    fn build_v2_compacted_history_expires_paths_when_current_window_has_no_images() {
        let old_path = "/tmp/old-window.png";
        let mut input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #1] path=\"{old_path}\">"),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,old".to_string(),
                        detail: None,
                    },
                    ContentItem::InputText {
                        text: "</image>".to_string(),
                    },
                    ContentItem::InputText {
                        text: "old image context".to_string(),
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            message("user", "current text only", /*phase*/ None),
        ];
        sanitize_compacted_media_prefix(&mut input, /*prefix_len*/ 1);
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) =
            build_v2_compacted_history(&input, /*compacted_prefix_len*/ 1, output.clone());

        assert_eq!(sanitization, CompactedMediaSanitization::default());
        assert_eq!(
            history,
            vec![
                message("user", "old image context", /*phase*/ None),
                message("user", "current text only", /*phase*/ None),
                output,
            ]
        );
    }

    #[test]
    fn retained_history_truncation_keeps_newest_messages_first() {
        let middle = message("user", "middle1234", /*phase*/ None);
        let new = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", "old-old", /*phase*/ None),
            middle,
            new.clone(),
        ];

        let truncated =
            truncate_retained_messages_for_remote_compaction(retained, /*max_tokens*/ 3);

        assert_eq!(
            truncated,
            vec![
                message("user", "midd…1 tokens truncated…1234", /*phase*/ None),
                new,
            ]
        );
    }

    #[test]
    fn retained_history_truncation_keeps_omission_fragments_atomic() {
        let omission = CompactedImageOmission::unavailable().render();
        let newest = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", "older", /*phase*/ None),
            message("user", omission.as_str(), /*phase*/ None),
            newest.clone(),
        ];

        let truncated =
            truncate_retained_messages_for_remote_compaction(retained, /*max_tokens*/ 2);

        assert_eq!(truncated, vec![newest]);
    }

    #[test]
    fn retained_history_truncation_drops_message_when_text_exhausts_marker_budget() {
        let omission = CompactedImageOmission::unavailable().render();
        let retained = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "abcdefgh".to_string(),
                },
                ContentItem::InputText { text: omission },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];

        let truncated =
            truncate_retained_messages_for_remote_compaction(retained, /*max_tokens*/ 1);

        assert_eq!(truncated, Vec::<ResponseItem>::new());
    }

    #[test]
    fn retained_history_truncation_keeps_local_image_reference_wrapper_atomic() {
        let retained = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "<image name=[Image #1] path=\"/tmp/context.png\">".to_string(),
                },
                ContentItem::InputText {
                    text: CompactedImageOmission::reopenable_local_image().render(),
                },
                ContentItem::InputText {
                    text: "</image>".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let wrapper_tokens = message_text_token_count(&retained[0]);

        let truncated = truncate_retained_messages_for_remote_compaction(
            retained,
            /*max_tokens*/ wrapper_tokens.saturating_sub(1),
        );

        assert_eq!(truncated, Vec::<ResponseItem>::new());
    }

    #[test]
    fn retained_history_truncation_preserves_images_and_truncates_later_text_parts() {
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "abcdef".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                },
                ContentItem::OutputText {
                    text: "uvwxyz".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };

        let truncated =
            truncate_retained_messages_for_remote_compaction(vec![item], /*max_tokens*/ 3);

        assert_eq!(
            truncated,
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "abcdef".to_string(),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,abc".to_string(),
                        detail: None,
                    },
                    ContentItem::OutputText {
                        text: "uv…1 tokens truncated…yz".to_string(),
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }]
        );
    }

    #[test]
    fn retained_history_truncation_charges_image_only_messages() {
        let image_only_message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let newest = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", "old", /*phase*/ None),
            image_only_message.clone(),
            newest.clone(),
        ];

        let truncated =
            truncate_retained_messages_for_remote_compaction(retained, /*max_tokens*/ 2);

        assert_eq!(truncated, vec![image_only_message, newest]);
    }

    #[test]
    fn retained_history_truncation_drops_image_only_messages_after_budget_is_spent() {
        let image_only_message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let newest = message("user", "new", /*phase*/ None);
        let retained = vec![image_only_message, newest.clone()];

        let truncated =
            truncate_retained_messages_for_remote_compaction(retained, /*max_tokens*/ 1);

        assert_eq!(truncated, vec![newest]);
    }

    #[tokio::test]
    async fn collect_compaction_output_accepts_additional_output_items() {
        let compaction = ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let stream = response_stream(vec![
            Ok(ResponseEvent::OutputItemDone(message(
                "assistant",
                "IGNORED_COMPACT_REPLY",
                Some(MessagePhase::FinalAnswer),
            ))),
            Ok(ResponseEvent::OutputItemDone(compaction.clone())),
            Ok(ResponseEvent::Completed {
                response_id: "resp-compact".to_string(),
                token_usage: Some(TokenUsage {
                    input_tokens: 123_456,
                    cached_input_tokens: 7_890,
                    cache_write_input_tokens: 0,
                    output_tokens: 42,
                    reasoning_output_tokens: 5,
                    total_tokens: 123_498,
                }),
                end_turn: Some(true),
            }),
        ]);

        let output = collect_compaction_output(stream)
            .await
            .expect("compaction should be collected");

        assert_eq!(output.compaction_output, compaction);
        assert_eq!(output.response_id, "resp-compact");
        assert_eq!(
            output.token_usage,
            Some(TokenUsage {
                input_tokens: 123_456,
                cached_input_tokens: 7_890,
                cache_write_input_tokens: 0,
                output_tokens: 42,
                reasoning_output_tokens: 5,
                total_tokens: 123_498,
            })
        );
    }
}
