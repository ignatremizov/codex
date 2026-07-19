use std::sync::Arc;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::compaction_status_from_result;
use crate::compact::insert_initial_context_before_last_real_user_or_summary;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote::should_keep_compacted_history_item;
use crate::compact_remote_history::HistoryItemGroup;
use crate::compact_remote_history::history_item_groups;
use crate::compacted_history_retention::RetainedMessageTruncation;
use crate::compacted_history_retention::truncate_retained_message_to_token_budget;
use crate::context::CompactedMediaSanitization;
use crate::context::annotated_compacted_image_omission;
use crate::context::compacted_image_omission_text;
use crate::context::expire_compacted_media_references;
use crate::context::sanitize_compacted_media;
use crate::context::standalone_compacted_image_omission_message;
use crate::context_manager::estimate_item_token_count;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_features::Feature;
#[cfg(test)]
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::CONTEXT_COMPACTION_DECODING_MESSAGE;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout_trace::CompactionCheckpointTracePayload;
use codex_rollout_trace::InferenceTraceContext;
use codex_utils_output_truncation::approx_token_count;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

// Mirror the current /responses/compact retained-message default while the
// server-side path remains the reference implementation.
pub(crate) const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 128_000;
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
    let step_context = sess
        .capture_step_context(Arc::clone(&turn_context), &CancellationToken::new())
        .await?;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.mode(),
        agent_queue: None,
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
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted) => Err(err),
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
        turn_context.model_info().slug.as_str(),
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
            sess.set_last_known_step_context(fallback_step_context)
                .await;
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace =
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    compaction_id.as_str(),
                    fallback_turn_context.model_info().slug.as_str(),
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
                turn_context.model_info().slug.as_str(),
                fallback_turn_context.model_info().slug.as_str(),
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
        replacement_history_input,
        compacted_prefix_len,
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        compaction_response_id,
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
    let explicit_mcp_context = prompt_input
        .iter()
        .cloned()
        .zip(prompt_input_metadata.iter().cloned())
        .map(|(item, metadata)| ResponseItemEnvelope { item, metadata })
        .filter(|envelope| {
            crate::context::McpServerUseInstructions::matches_response_item(&envelope.item)
        })
        .collect();
    let (compacted_history, media_sanitization) = build_v2_compacted_history(
        &replacement_history_input,
        compacted_prefix_len,
        compaction_output,
        sess.enabled(Feature::RetainClientDeveloperMessages),
    );
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
    let (initial_context, world_state_baseline) =
        build_compaction_initial_context(sess.as_ref(), &initial_context_injection).await;
    let new_history =
        insert_initial_context_before_last_real_user_or_summary(compacted_history, initial_context);
    let new_history =
        crate::compact::insert_annotated_mcp_server_use_context_items_at_compaction_boundary(
            new_history,
            explicit_mcp_context,
        );

    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { .. } => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    let final_history = sess
        .replace_compacted_history(
            Arc::clone(compaction_turn_context),
            new_history,
            reference_context_item,
            world_state_baseline,
            CompactedHistoryMetadata {
                message: String::new(),
                compaction_summary_tokens,
                window_number: new_window_number,
                window_ids: new_window_ids,
                compaction_response_id: Some(compaction_response_id),
            },
        )
        .await?;
    let final_history_items = final_history
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    if let Some(trace_input_history) = trace_input_history.as_deref() {
        compaction_trace.record_installed(&CompactionCheckpointTracePayload {
            input_history: trace_input_history,
            replacement_history: &final_history_items,
        });
    }
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
    crate::compact_handoff_summary::summarize_remote_compaction_handoff(
        sess,
        compaction_turn_context,
        &final_history_items,
        cancellation_token,
    )
    .await
    .apply_to(&mut context_compaction_item);
    context_compaction_item.available_skills =
        crate::compact_skills_inventory::available_skill_names(&final_history_items);

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
    step_context: &StepContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
) -> CodexResult<RemoteCompactionV2Output> {
    let turn_context = &step_context.turn;
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retry_state = ResponsesStreamRetryState::default();
    loop {
        let result = match client_session
            .stream(
                prompt,
                turn_context.model_info(),
                &turn_context.session_telemetry,
                turn_context.reasoning_effort().cloned(),
                turn_context.reasoning_summary(),
                step_context.settings.service_tier.clone(),
                responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
        {
            Ok(stream) => collect_compaction_output(sess, turn_context, stream).await,
            Err(err) => Err(err),
        };

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                handle_retryable_response_stream_error(
                    &mut retry_state,
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
    sess: &Session,
    turn_context: &TurnContext,
    mut stream: ResponseStream,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut compaction_output = None;
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
                usage_metadata,
                ..
            } => {
                sess.record_observed_response_completed(
                    turn_context,
                    &response_id,
                    token_usage.as_ref(),
                    usage_metadata.as_ref(),
                )
                .await;
                completed_response_id = Some(response_id);
                completed_token_usage = token_usage;
                break;
            }
            _ => {}
        }
    }

    let Some(response_id) = completed_response_id else {
        return Err(CodexErr::Stream(
            "remote compaction v2 stream closed before response.completed".to_string(),
        ));
    };

    if compaction_count != 1 {
        return Err(CodexErr::Fatal(format!(
            "remote compaction v2 expected exactly one compaction output item, got {compaction_count} from {output_item_count} output items"
        )));
    }

    let Some(compaction_output) = compaction_output else {
        unreachable!("compaction output must exist when count is exactly one");
    };
    Ok(RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage: completed_token_usage,
    })
}

fn build_v2_compacted_history(
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
            if !should_keep_compacted_history_item(&envelope.item) {
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
        .filter(|group| is_retained_for_remote_compaction_v2(&group.source.item))
        .filter(|group| {
            should_keep_compacted_history_item(&group.source.item)
                || (retain_client_developer_messages
                    && is_client_authored_developer_message(&group.source))
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

pub(crate) fn is_client_authored_developer_message(item: &ResponseItemEnvelope) -> bool {
    item.metadata
        .as_ref()
        .is_some_and(|metadata| metadata.client_authored)
        && matches!(&item.item, ResponseItem::Message { role, .. } if role == "developer")
}

fn v2_history_item_groups(
    items: Vec<ResponseItemEnvelope>,
) -> impl Iterator<Item = HistoryItemGroup<ResponseItemEnvelope>> {
    history_item_groups(items).flat_map(|mut group| {
        let client_message = group
            .attached_notice
            .take_if(|item| is_client_authored_developer_message(item))
            .map(|source| HistoryItemGroup {
                source,
                attached_notice: None,
            });
        std::iter::once(group).chain(client_message)
    })
}

fn is_retained_for_remote_compaction_v2(item: &ResponseItem) -> bool {
    if matches!(item, ResponseItem::AgentMessage { .. }) {
        // Agent communication is auditable working context. Keep progress and completion records
        // subject to the same bounded retained-history budget as every other retained item.
        return true;
    }

    let ResponseItem::Message { role, .. } = item else {
        return false;
    };

    matches!(role.as_str(), "user" | "developer" | "system")
}

pub(crate) fn truncate_retained_messages_for_remote_compaction(
    items: Vec<ResponseItemEnvelope>,
    max_tokens: usize,
) -> Vec<ResponseItemEnvelope> {
    let mut remaining = max_tokens;
    let mut truncated_reversed = Vec::with_capacity(items.len());
    for group in v2_history_item_groups(items)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if remaining == 0 {
            continue;
        }

        let client_developer = is_client_authored_developer_message(&group.source);
        let notice_tokens = group
            .attached_notice
            .as_ref()
            .map_or(0, |notice| message_text_token_count(&notice.item).max(1));
        let source_tokens = if client_developer {
            usize::try_from(estimate_item_token_count(&group.source.item)).unwrap_or(usize::MAX)
        } else {
            message_text_token_count(&group.source.item).max(1)
        };
        let token_count = source_tokens.saturating_add(notice_tokens);
        if token_count <= remaining {
            if let Some(notice) = group.attached_notice {
                truncated_reversed.push(notice);
            }
            truncated_reversed.push(group.source);
            remaining = remaining.saturating_sub(token_count);
        } else if remaining > notice_tokens {
            let available_tokens = remaining - notice_tokens;
            let ResponseItemEnvelope { item, metadata } = group.source;
            match truncate_retained_message_to_token_budget(
                item,
                /*max_tokens*/ available_tokens,
            ) {
                RetainedMessageTruncation::Retained(truncated_item) => {
                    let truncated_item = ResponseItemEnvelope {
                        item: *truncated_item,
                        metadata,
                    };
                    if client_developer
                        && usize::try_from(estimate_item_token_count(&truncated_item.item))
                            .unwrap_or(usize::MAX)
                            > available_tokens
                    {
                        remaining = 0;
                        continue;
                    }
                    if let Some(notice) = group.attached_notice {
                        truncated_reversed.push(notice);
                    }
                    truncated_reversed.push(truncated_item);
                    remaining = 0;
                }
                RetainedMessageTruncation::OmissionDidNotFit => remaining = 0,
                RetainedMessageTruncation::Empty => remaining = 0,
            }
        } else {
            remaining = 0;
        }
    }
    truncated_reversed.reverse();
    truncated_reversed
}

fn message_text_token_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return usize::try_from(estimate_item_token_count(item)).unwrap_or(usize::MAX);
    };

    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                approx_token_count(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CompactedImageOmission;
    use crate::context::ContextualUserFragment;
    use crate::context::sanitize_compacted_media_prefix;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ContentItemKind;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;
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

    fn message_with_kinds(
        role: &str,
        content: Vec<ContentItem>,
        content_item_kinds: Vec<&str>,
    ) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(
                        content_item_kinds
                            .into_iter()
                            .map(|kind| ContentItemKind(kind.to_string()))
                            .collect(),
                    ),
                    ..Default::default()
                },
            ),
        }
    }

    fn build_without_metadata(
        input: Vec<ResponseItem>,
        output: ResponseItem,
    ) -> (Vec<ResponseItemEnvelope>, CompactedMediaSanitization) {
        build_without_metadata_with_prefix(input, /*compacted_prefix_len*/ 0, output)
    }

    fn build_without_metadata_with_prefix(
        input: Vec<ResponseItem>,
        compacted_prefix_len: usize,
        output: ResponseItem,
    ) -> (Vec<ResponseItemEnvelope>, CompactedMediaSanitization) {
        let input = annotated(input);
        build_v2_compacted_history(
            &input,
            compacted_prefix_len,
            output,
            /*retain_client_developer_messages*/ false,
        )
    }

    fn annotated(items: Vec<ResponseItem>) -> Vec<ResponseItemEnvelope> {
        items.into_iter().map(ResponseItemEnvelope::new).collect()
    }

    fn raw(items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItem> {
        items
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect()
    }

    fn truncate_without_metadata(items: Vec<ResponseItem>, max_tokens: usize) -> Vec<ResponseItem> {
        raw(truncate_retained_messages_for_remote_compaction(
            annotated(items),
            max_tokens,
        ))
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
                encrypted_function_args: None,
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

        let (history, _) = build_without_metadata(input, output.clone());

        assert_eq!(
            raw(history),
            vec![message("user", "user", /*phase*/ None), output]
        );
    }

    #[test]
    fn build_v2_compacted_history_preserves_retained_metadata_sidecar() {
        let retained = message("user", "keep", /*phase*/ None);
        let generated_notice = message(
            "developer",
            "<image_resize_notice>generated</image_resize_notice>",
            /*phase*/ None,
        );
        let harness = message("developer", "drop", /*phase*/ None);
        let client = message(
            "developer",
            "<image_resize_notice>client</image_resize_notice>",
            /*phase*/ None,
        );
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        for enabled in [false, true] {
            let input = vec![
                ResponseItemEnvelope::new(harness.clone()),
                ResponseItemEnvelope {
                    item: client.clone(),
                    metadata: Some(CodexHarnessMetadata {
                        client_authored: true,
                        ..Default::default()
                    }),
                },
                ResponseItemEnvelope {
                    item: retained.clone(),
                    metadata: Some(CodexHarnessMetadata::default()),
                },
                ResponseItemEnvelope::new(generated_notice.clone()),
            ];
            let (history, _) = build_v2_compacted_history(
                &input,
                /*compacted_prefix_len*/ 0,
                output.clone(),
                enabled,
            );
            let mut expected = vec![
                ResponseItemEnvelope {
                    item: retained.clone(),
                    metadata: Some(CodexHarnessMetadata::default()),
                },
                ResponseItemEnvelope::new(generated_notice.clone()),
                ResponseItemEnvelope::new(output.clone()),
            ];
            if enabled {
                expected.insert(
                    0,
                    ResponseItemEnvelope {
                        item: client.clone(),
                        metadata: Some(CodexHarnessMetadata {
                            client_authored: true,
                            ..Default::default()
                        }),
                    },
                );
            }
            assert_eq!(history, expected);
        }
    }

    #[test]
    fn retained_history_truncation_preserves_metadata() {
        let item = ResponseItemEnvelope {
            item: message("user", "word ".repeat(200).as_str(), /*phase*/ None),
            metadata: Some(CodexHarnessMetadata::default()),
        };
        let original_item = item.item.clone();

        let truncated =
            truncate_retained_messages_for_remote_compaction(vec![item], /*max_tokens*/ 32);

        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].metadata, Some(CodexHarnessMetadata::default()));
        assert_ne!(truncated[0].item, original_item);
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

        let (history, _) = build_without_metadata(input, output.clone());

        assert_eq!(raw(history), vec![old, new, output]);
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

        let (history, sanitization) = build_without_metadata(input, output);

        assert_eq!(sanitization.omitted_image_count, 2);
        assert_eq!(sanitization.omitted_inline_media_bytes, 50);
        assert!(
            history.iter().all(|item| {
                !matches!(
                    &item.item,
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
                call_id: Some("tool-call".to_string()),
                name: None,
                namespace: None,
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

        let (history, _) = build_without_metadata(input, output.clone());
        let history = raw(history);

        assert_eq!(
            history,
            vec![
                message_with_kinds(
                    "user",
                    vec![
                        ContentItem::InputText {
                            text: "user".to_string(),
                        },
                        ContentItem::InputText { text: omission },
                    ],
                    vec!["unknown", "compaction.image_omission"],
                ),
                output,
            ]
        );
    }

    #[test]
    fn build_v2_compacted_history_retains_tool_only_current_window_omission() {
        let input = vec![ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("tool-call".to_string()),
            name: None,
            namespace: None,
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

        let (history, sanitization) = build_without_metadata(input, output.clone());
        let history = raw(history);

        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            history,
            vec![
                message_with_kinds(
                    "developer",
                    vec![ContentItem::InputText {
                        text: CompactedImageOmission::unavailable().render(),
                    }],
                    vec!["compaction.image_omission"],
                ),
                output,
            ]
        );
        assert!(should_keep_compacted_history_item(&history[0]));
        assert!(!crate::context_manager::is_user_turn_boundary(&history[0]));
    }

    #[test]
    fn build_v2_compacted_history_rehomes_omission_after_budget_truncation() {
        let input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputImage {
                    image_url: "data:image/png;base64,current".to_string(),
                    detail: None,
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            message(
                "user",
                "x".repeat(RETAINED_MESSAGE_TOKEN_BUDGET.saturating_mul(4))
                    .as_str(),
                /*phase*/ None,
            ),
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) = build_without_metadata(input, output.clone());
        let history = raw(history);
        let retained = &history[..history.len().saturating_sub(1)];
        let omission = CompactedImageOmission::unavailable().render();

        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            compacted_image_omission_text(retained),
            Some(omission.as_str())
        );
        assert!(
            retained.iter().map(message_text_token_count).sum::<usize>()
                <= RETAINED_MESSAGE_TOKEN_BUDGET
        );
        assert_eq!(history.last(), Some(&output));
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
            sanitize_compacted_media_prefix(&mut input, /*prefix_len*/ 1);
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, sanitization) = build_without_metadata_with_prefix(
            input,
            /*compacted_prefix_len*/ 2,
            output.clone(),
        );
        let history = raw(history);

        assert_eq!(pre_compaction_sanitization.omitted_image_count, 1);
        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            history,
            vec![
                message_with_kinds(
                    "user",
                    vec![ContentItem::InputText {
                        text: "old image context".to_string(),
                    }],
                    vec!["unknown"],
                ),
                message_with_kinds(
                    "user",
                    vec![
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
                    vec!["unknown", "compaction.image_omission", "unknown", "unknown",],
                ),
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

        let (history, sanitization) = build_without_metadata_with_prefix(
            input,
            /*compacted_prefix_len*/ 1,
            output.clone(),
        );
        let history = raw(history);

        assert_eq!(sanitization, CompactedMediaSanitization::default());
        assert_eq!(
            history,
            vec![
                message_with_kinds(
                    "user",
                    vec![ContentItem::InputText {
                        text: "old image context".to_string(),
                    }],
                    vec!["unknown"],
                ),
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

        let truncated = truncate_without_metadata(retained, /*max_tokens*/ 3);

        assert_eq!(truncated, vec![new]);
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

        let truncated = raw(truncate_retained_messages_for_remote_compaction(
            annotated(retained),
            /*max_tokens*/ 2,
        ));

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

        let truncated = raw(truncate_retained_messages_for_remote_compaction(
            annotated(retained),
            /*max_tokens*/ 1,
        ));

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

        let truncated = raw(truncate_retained_messages_for_remote_compaction(
            annotated(retained),
            /*max_tokens*/ wrapper_tokens.saturating_sub(1),
        ));

        assert_eq!(truncated, Vec::<ResponseItem>::new());
    }

    #[test]
    fn retained_history_truncation_preserves_images_when_later_text_marker_does_not_fit() {
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
                ContentItem::InputText {
                    text: "discarded after the text budget is exhausted".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,def".to_string(),
                    detail: None,
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-1".to_string()),
                    content_item_kinds: Some(vec![
                        ContentItemKind("user.text".to_string()),
                        ContentItemKind("user.image".to_string()),
                        ContentItemKind("user.text".to_string()),
                        ContentItemKind("user.text".to_string()),
                        ContentItemKind("user.image".to_string()),
                    ]),
                    ..Default::default()
                },
            ),
        };

        let truncated = truncate_without_metadata(vec![item], /*max_tokens*/ 3);

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
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        turn_id: Some("turn-1".to_string()),
                        content_item_kinds: Some(vec![
                            ContentItemKind("user.text".to_string()),
                            ContentItemKind("user.image".to_string()),
                        ]),
                        ..Default::default()
                    },
                ),
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

        let truncated = truncate_without_metadata(retained, /*max_tokens*/ 2);

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

        let truncated = truncate_without_metadata(retained, /*max_tokens*/ 1);

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
                    codex_rollout_budget_units: None,
                }),
                usage_metadata: Some(codex_protocol::ResponseUsageMetadata {
                    amount: Some("0.125".to_string()),
                    metadata: Some(serde_json::json!({ "extra": { "label": "example" } })),
                }),
                end_turn: Some(true),
            }),
        ]);

        let (sess, turn_context, rx) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let output = collect_compaction_output(&sess, &turn_context, stream)
            .await
            .expect("compaction should be collected");

        assert_eq!(output.compaction_output, compaction);
        assert_eq!(output.response_id, "resp-compact");
        let event = rx.recv().await.expect("raw response completion");
        let EventMsg::RawResponseCompleted(completed) = event.msg else {
            panic!("expected raw response completion, got {:?}", event.msg);
        };
        assert_eq!(completed.response_id, "resp-compact");
        assert_eq!(
            completed.usage_metadata,
            Some(codex_protocol::ResponseUsageMetadata {
                amount: Some("0.125".to_string()),
                metadata: Some(serde_json::json!({ "extra": { "label": "example" } })),
            })
        );
        assert_eq!(
            output.token_usage,
            Some(TokenUsage {
                input_tokens: 123_456,
                cached_input_tokens: 7_890,
                cache_write_input_tokens: 0,
                output_tokens: 42,
                reasoning_output_tokens: 5,
                total_tokens: 123_498,
                codex_rollout_budget_units: None,
            })
        );
    }
}
