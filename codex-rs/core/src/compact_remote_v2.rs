use std::sync::Arc;
use std::sync::OnceLock;

use crate::compact_retained_tool_outputs::is_retained_tool_output;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client::ResponsesHttpExecutor;
use crate::client_common::ResponseEvent;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::compaction_status_from_result;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote_history::HistoryItemGroup;
use crate::compact_remote_history::history_item_groups;
use crate::compacted_history_retention::RetainedMessageTruncation;
use crate::compacted_history_retention::truncate_retained_message_to_token_budget;
use crate::context::CompactedMediaSanitization;
use crate::context::McpServerUseInstructions;
use crate::context::annotated_compacted_image_omission;
use crate::context::compacted_image_omission_text;
use crate::context::expire_compacted_media_references;
#[cfg(test)]
use crate::context::sanitize_compacted_media;
use crate::context::standalone_compacted_image_omission_message;
use crate::context::world_state::WorldState;
use crate::context_manager::estimate_item_token_count;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_response_stream_error;
use crate::session::RequestEffortUsage;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_features::Feature;
use codex_features::Features;
#[cfg(test)]
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
#[cfg(test)]
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::InferenceTraceContext;
use codex_utils_output_truncation::approx_token_count;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
pub(crate) use attempt::BackgroundCompactionCandidate;
use attempt::RemoteCompactV2Attempt;
use attempt::RemoteCompactV2AttemptInput;
pub(crate) use attempt::capture_background_input;
pub(crate) use attempt::run_background_compaction;
use attempt::run_remote_compact_v2_attempt;

#[path = "compact_remote_v2_installation.rs"]
mod installation;
use installation::run_compaction;

pub(crate) const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 128_000;
const MAX_RETAINED_AGENT_MESSAGE_TOKENS: i64 = 10_000;
// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;

#[derive(Clone, Copy)]
enum CompactionRequestKind<'a> {
    Foreground,
    Background(&'a CancellationToken),
}

enum CompactionRequestTransport<'a> {
    Foreground(&'a mut ModelClientSession),
    Background(Option<String>, Option<u64>),
}

/// The caller decides whether a declined prepared result needs a foreground request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionFallback {
    Foreground,
    Skip,
}

#[derive(Clone, Copy)]
pub(crate) struct AutoCompactRun<'a> {
    pub(crate) reason: CompactionReason,
    pub(crate) phase: CompactionPhase,
    pub(crate) cancellation: &'a CancellationToken,
}

#[derive(Clone, Copy)]
struct RemoteCompactRun<'a> {
    metadata: CompactionTurnMetadata,
    cancellation: &'a CancellationToken,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    replacement_step_context: Arc<StepContext>,
    client_session: &mut ModelClientSession,
    world_state: Arc<WorldState>,
    run: AutoCompactRun<'_>,
    candidate: Option<Box<BackgroundCompactionCandidate>>,
    fallback: CompactionFallback,
) -> CodexResult<bool> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        run.reason,
        CompactionImplementation::ResponsesCompactionV2,
        run.phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        &replacement_step_context,
        Some(client_session),
        world_state,
        RemoteCompactRun {
            metadata: compaction_metadata,
            cancellation: run.cancellation,
        },
        candidate,
        fallback,
    )
    .await
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    world_state: Arc<WorldState>,
    cancellation: &CancellationToken,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompactionV2,
        CompactionPhase::StandaloneTurn,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        &step_context,
        /*client_session*/ None,
        world_state,
        RemoteCompactRun {
            metadata: compaction_metadata,
            cancellation,
        },
        /*candidate*/ None,
        CompactionFallback::Foreground,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    replacement_step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    world_state: Arc<WorldState>,
    run: RemoteCompactRun<'_>,
    candidate: Option<Box<BackgroundCompactionCandidate>>,
    fallback: CompactionFallback,
) -> CodexResult<bool> {
    let compaction_metadata = run.metadata;
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
    let started_item = if candidate.is_none() {
        let item = TurnItem::ContextCompaction(ContextCompactionItem::new());
        sess.emit_turn_item_started(turn_context, &item).await;
        Some(item)
    } else {
        None
    };
    let candidate = if candidate.is_none() && fallback == CompactionFallback::Foreground {
        crate::session::async_compaction::wait_for_pending(sess, step_context).await
    } else {
        candidate
    };
    // Box the combined request/install future to keep it off the turn stack.
    let result = Box::pin(run_compaction(
        sess,
        step_context,
        replacement_step_context,
        client_session,
        &world_state,
        run,
        &mut analytics_details,
        started_item.as_ref(),
        candidate.map(|candidate| *candidate),
        fallback,
    ))
    .await;
    if matches!(result, Ok(false)) {
        if let Some(item) = started_item {
            sess.emit_turn_item_completed(turn_context, item).await;
        }
        return Ok(false);
    }
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
        Ok(installed) => Ok(installed),
        Err(err)
            if matches!(err.details(), CodexErrorDetails::TurnAborted)
                || matches!(phase, CompactionPhase::PostTurn) =>
        {
            Err(err)
        }
        Err(err) => {
            sess.track_turn_codex_error(turn_context, &err);
            // Pre-turn failures are reported by run_turn after preserving the incoming prompt.
            if !matches!(phase, CompactionPhase::PreTurn) {
                let event = EventMsg::Error(
                    err.to_error_event(Some("Error running remote compact task".to_string())),
                );
                sess.send_event(turn_context, event).await;
            }
            Err(err)
        }
    }
}

struct RemoteCompactionV2Output {
    compaction_output: ResponseItem,
    response_id: String,
    token_usage: Option<TokenUsage>,
}

async fn run_remote_compaction_request_v2(
    sess: &Session,
    step_context: &StepContext,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
    transport: CompactionRequestTransport<'_>,
) -> CodexResult<RemoteCompactionV2Output> {
    let turn_context = &step_context.turn;
    let client_session = match transport {
        CompactionRequestTransport::Background(turn_state, auth_owner_generation) => {
            let settings = &step_context.settings;
            let cancellation = crate::session::async_compaction::shutdown_cancellation(sess).await;
            let effort = sess
                .reasoning_effort_for_request(settings, RequestEffortUsage::Compaction)
                .await;
            let trace = InferenceTraceContext::disabled();
            // Replay the routing token without letting this request change foreground routing.
            let turn_state = Arc::new(turn_state.map(OnceLock::from).unwrap_or_default());
            let http_executor = ResponsesHttpExecutor::new(&sess.services.model_client);
            let stream = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                stream = http_executor.stream_responses_http(
                    &turn_state, auth_owner_generation, /*event_sender*/ None, prompt, &settings.model_info, &step_context.session_telemetry, effort,
                    settings.reasoning_summary, settings.service_tier.clone(), responses_metadata,
                    &trace,
                ) => stream?,
            };
            return collect_compaction_output(
                sess,
                turn_context,
                stream,
                CompactionRequestKind::Background(&cancellation),
            )
            .await;
        }
        CompactionRequestTransport::Foreground(client_session) => client_session,
    };
    let settings = &turn_context.initial_settings;
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retry_state = ResponsesStreamRetryState::default();
    loop {
        sess.await_history_publication().await;
        sess.check_history_publication()?;
        let result = match client_session
            .stream(
                prompt,
                &settings.model_info,
                &turn_context.session_telemetry,
                sess.reasoning_effort_for_request(settings, RequestEffortUsage::Compaction)
                    .await,
                settings.reasoning_summary,
                step_context.settings.service_tier.clone(),
                responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
        {
            Ok(stream) => {
                collect_compaction_output(
                    sess,
                    turn_context,
                    stream,
                    CompactionRequestKind::Foreground,
                )
                .await
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) => {
                handle_response_stream_error(
                    &mut retry_state,
                    max_retries,
                    err,
                    client_session,
                    sess,
                    step_context,
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
    request_kind: CompactionRequestKind<'_>,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut compaction_output = None;
    let mut completed_response_id = None;
    let mut completed_token_usage = None;
    loop {
        let event = match request_kind {
            CompactionRequestKind::Foreground => stream.next().await,
            CompactionRequestKind::Background(cancellation) => tokio::select! {
                biased;
                // Charge completed usage even when cancellation arrives at the same time.
                event = stream.next() => event,
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            },
        };
        let Some(event) = event else { break };
        if let Err(err) = &event
            && let CodexErrorDetails::ResponseIncomplete(incomplete) = err.details()
        {
            match request_kind {
                CompactionRequestKind::Background(_) => {
                    if let Some(usage) = incomplete.token_usage.as_ref() {
                        // Match completed background requests without replacing the terminal error.
                        let _ = sess.record_rollout_budget_usage(usage).await;
                    }
                }
                CompactionRequestKind::Foreground => {
                    sess.record_incomplete_response_usage(
                        turn_context,
                        &turn_context.initial_settings,
                        incomplete,
                    )
                    .await;
                    sess.send_token_count_event(turn_context).await;
                }
            }
        }
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
                match request_kind {
                    CompactionRequestKind::Background(_) => {
                        if let Some(usage) = token_usage.as_ref() {
                            sess.record_rollout_budget_usage(usage).await?;
                        }
                    }
                    CompactionRequestKind::Foreground => {
                        sess.record_observed_response_completed(
                            turn_context,
                            &response_id,
                            token_usage.as_ref(),
                            usage_metadata.as_ref(),
                        )
                        .await;
                    }
                }
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

#[path = "compact_remote_v2_media.rs"]
mod media;
use media::build_v2_compacted_history;

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

fn is_retained_for_remote_compaction_v2(
    envelope: &ResponseItemEnvelope,
    features: &Features,
) -> bool {
    if features.enabled(Feature::RetainClientToolOutputs) && is_retained_tool_output(envelope) {
        return true;
    }
    let item = &envelope.item;
    if let ResponseItem::AgentMessage {
        author,
        recipient,
        content,
        ..
    } = item
    {
        let is_descendant_progress = author
            .strip_prefix(recipient)
            .is_some_and(|suffix| suffix.starts_with('/'))
            && matches!(
                content.first(),
                Some(AgentMessageInputContent::InputText { text })
                    if text.starts_with("Message Type: MESSAGE\n")
                        || text.starts_with("Message Type: CHANNEL_POST\n")
            );
        let is_completion = matches!(
            content.first(),
            Some(AgentMessageInputContent::InputText { text })
                if text.starts_with("Message Type: FINAL_ANSWER\n")
        );
        return !is_descendant_progress
            && !is_completion
            && estimate_item_token_count(item) <= MAX_RETAINED_AGENT_MESSAGE_TOKENS;
    }

    let ResponseItem::Message { role, .. } = item else {
        return false;
    };

    match role.as_str() {
        "user" => matches!(
            crate::event_mapping::parse_turn_item(item),
            Some(TurnItem::UserMessage(_) | TurnItem::HookPrompt(_))
        ),
        "developer" => {
            McpServerUseInstructions::matches_response_item(item)
                || crate::context::is_standalone_compacted_image_omission_message(item)
                || (features.enabled(Feature::RetainClientDeveloperMessages)
                    && is_client_authored_developer_message(envelope))
        }
        _ => false,
    }
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
        // Explicitly requested inventories retain every accepted envelope, outside this budget.
        if McpServerUseInstructions::matches_response_item(&group.source.item) {
            if let Some(notice) = group.attached_notice {
                truncated_reversed.push(notice);
            }
            truncated_reversed.push(group.source);
            continue;
        }
        if remaining == 0 {
            continue;
        }

        let client_developer = is_client_authored_developer_message(&group.source);
        let notice_tokens = group
            .attached_notice
            .as_ref()
            .map_or(0, |notice| message_text_token_count(&notice.item).max(1));
        let source_tokens = if client_developer || is_retained_tool_output(&group.source) {
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
        } else if is_retained_tool_output(&group.source) {
            // Drop older budgeted items while still preserving explicit MCP inventories.
            remaining = 0;
        } else if remaining > notice_tokens {
            let available_tokens = remaining - notice_tokens;
            let content_budget = if client_developer {
                available_tokens.saturating_sub(
                    source_tokens.saturating_sub(message_text_token_count(&group.source.item)),
                )
            } else {
                available_tokens
            };
            let ResponseItemEnvelope { item, mut metadata } = group.source;
            let mut truncated = truncate_retained_message_to_token_budget(item, content_budget);
            if client_developer && let RetainedMessageTruncation::Retained(item) = &truncated {
                let item_tokens =
                    usize::try_from(estimate_item_token_count(item)).unwrap_or(usize::MAX);
                if item_tokens > available_tokens {
                    truncated = truncate_retained_message_to_token_budget(
                        item.as_ref().clone(),
                        content_budget
                            .saturating_sub(item_tokens - available_tokens)
                            .saturating_sub(1),
                    );
                }
            }
            match truncated {
                RetainedMessageTruncation::Retained(truncated_item) => {
                    if let Some(metadata) = &mut metadata {
                        metadata.mark_retained_sources_incomplete();
                    }
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
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                approx_token_count(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => 0,
        })
        .sum()
}

#[cfg(test)]
#[path = "compact_remote_v2_context_tests.rs"]
mod context_tests;

#[cfg(test)]
#[path = "compact_remote_v2_mcp_tests.rs"]
mod mcp_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compacted_history_retention::truncate_text_to_approx_token_budget;
    use crate::context::CompactedImageOmission;
    use crate::context::sanitize_compacted_media_prefix;
    use codex_context_fragments::ContextualUserFragment;
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
            status: None,
            encrypted_content: None,
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
            status: None,
            encrypted_content: None,
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
        let input = annotated(input);
        build_v2_compacted_history(
            &input,
            /*compacted_prefix_len*/ 0,
            output,
            &Features::with_defaults(),
        )
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
            &Features::with_defaults(),
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
            interrupt: None,
            consumer_dropped: CancellationToken::new(),
        }
    }

    #[test]
    fn build_v2_compacted_history_filters_to_installed_retention_shape() {
        let hook = codex_protocol::items::build_hook_prompt_message(&[
            codex_protocol::items::HookPromptFragment::from_single_hook("hook", "hook-run"),
        ])
        .expect("hook prompt");
        let input = vec![
            message("developer", "dev", /*phase*/ None),
            message("system", "sys", /*phase*/ None),
            message("user", "user", /*phase*/ None),
            hook.clone(),
            message("assistant", "commentary", Some(MessagePhase::Commentary)),
            message("assistant", "final", Some(MessagePhase::FinalAnswer)),
            ResponseItem::FunctionCall {
                status: None,
                encrypted_content: None,
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
            vec![message("user", "user", /*phase*/ None), hook, output]
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
                ResponseItemEnvelope {
                    item: harness.clone(),
                    metadata: None,
                },
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
                ResponseItemEnvelope {
                    item: generated_notice.clone(),
                    metadata: None,
                },
            ];
            let mut features = Features::with_defaults();
            if enabled {
                features.enable(Feature::RetainClientDeveloperMessages);
            }
            let (history, _) = build_v2_compacted_history(
                &input,
                /*compacted_prefix_len*/ 0,
                output.clone(),
                &features,
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
        let text = "word ".repeat(200);
        let item = ResponseItemEnvelope {
            item: message("user", &text, /*phase*/ None),
            metadata: Some(CodexHarnessMetadata::default()),
        };

        // An omission marker cannot fit this budget; retaining it would exceed the limit.
        assert!(
            truncate_retained_messages_for_remote_compaction(
                vec![item.clone()],
                /*max_tokens*/ 4,
            )
            .is_empty()
        );
        let truncated =
            truncate_retained_messages_for_remote_compaction(vec![item], /*max_tokens*/ 64);

        assert_eq!(
            truncated,
            vec![ResponseItemEnvelope {
                item: message_with_kinds(
                    "user",
                    vec![ContentItem::InputText {
                        text: truncate_text_to_approx_token_budget(&text, /*max_tokens*/ 64),
                    }],
                    vec!["unknown"],
                ),
                metadata: Some(CodexHarnessMetadata::default()),
            }]
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

        let (history, _) = build_without_metadata(input, output.clone());

        assert_eq!(raw(history), vec![old, new, output]);
    }

    #[test]
    fn build_v2_compacted_history_counts_retained_input_images() {
        let input = vec![ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "user".to_string(),
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,abc".to_string(),
                    },
                    detail: None,
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,def".to_string(),
                    },
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
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "user".to_string(),
                    },
                    ContentItem::InputImage {
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,user".to_string(),
                        },
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
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,tool".to_string(),
                        },
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
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,tool".to_string(),
                    },
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
        assert!(is_retained_for_remote_compaction_v2(
            &ResponseItemEnvelope::new(history[0].clone()),
            &Features::with_defaults(),
        ));
        assert!(!crate::context_manager::is_user_turn_boundary(&history[0]));
    }

    #[test]
    fn build_v2_compacted_history_rehomes_omission_after_budget_truncation() {
        let input = vec![
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,current".to_string(),
                    },
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
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #1] path=\"{old_path}\">"),
                    },
                    ContentItem::InputImage {
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,old".to_string(),
                        },
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
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #2] path=\"{current_path}\">"),
                    },
                    ContentItem::InputImage {
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,current".to_string(),
                        },
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
                status: None,
                encrypted_content: None,
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: format!("<image name=[Image #1] path=\"{old_path}\">"),
                    },
                    ContentItem::InputImage {
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,old".to_string(),
                        },
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
        let middle_text = "middle1234".repeat(20);
        let middle = message("user", &middle_text, /*phase*/ None);
        let new = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", "old-old", /*phase*/ None),
            middle,
            new.clone(),
        ];

        let truncated = truncate_without_metadata(retained, /*max_tokens*/ 32);

        assert_eq!(
            truncated,
            vec![
                ResponseItem::Message {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: truncate_text_to_approx_token_budget(
                            &middle_text,
                            32 - approx_token_count("new"),
                        ),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: Some(
                        InternalChatMessageMetadataPassthrough {
                            content_item_kinds: Some(vec![ContentItemKind("unknown".to_string())]),
                            ..Default::default()
                        },
                    ),
                },
                new,
            ]
        );
    }

    #[test]
    fn retained_history_truncation_preserves_media_before_cutoff_and_aligns_metadata() {
        let later_text = "uvwxyz".repeat(100);
        let item = ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "abcdef".to_string(),
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,abc".to_string(),
                    },
                    detail: None,
                },
                ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: later_text.clone(),
                },
                ContentItem::InputText {
                    text: "discarded after the text budget is exhausted".to_string(),
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,def".to_string(),
                    },
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

        let source = codex_history::RetainedSource {
            id: codex_history::RetainedSourceId {
                message_id: "original".to_owned(),
                turn_id: "parent".to_owned(),
                role: codex_history::RetainedSourceRole::User,
            },
            revision: codex_protocol::ResponseItemId::from_server("revision-1".to_owned()),
            complete: true,
        };
        let mut metadata = CodexHarnessMetadata {
            retained_source: Some(source.clone()),
            guardian_sources: vec![source],
            ..Default::default()
        };
        let truncated = truncate_retained_messages_for_remote_compaction(
            vec![ResponseItemEnvelope {
                item,
                metadata: Some(metadata.clone()),
            }],
            /*max_tokens*/ 32,
        )
        .into_iter()
        .next()
        .expect("retained source");
        metadata.mark_retained_sources_incomplete();
        assert_eq!(truncated.metadata, Some(metadata));
        let truncated = vec![truncated.item];

        assert_eq!(
            truncated,
            vec![ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "abcdef".to_string(),
                    },
                    ContentItem::InputImage {
                        image: ImageReference::Inline {
                            image_url: "data:image/png;base64,abc".to_string()
                        },
                        detail: None,
                    },
                    ContentItem::OutputText {
                        annotations: None,
                        logprobs: None,
                        text: truncate_text_to_approx_token_budget(
                            &later_text,
                            32 - approx_token_count("abcdef"),
                        ),
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
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,abc".to_string(),
                },
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
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,abc".to_string(),
                },
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
    async fn collect_background_incomplete_does_not_publish_foreground_usage() {
        let stream = response_stream(vec![Err(CodexErr::new(
            CodexErrorDetails::ResponseIncomplete(Box::new(
                codex_protocol::error::ResponseIncompleteError {
                    reason: "max_output_tokens".to_string(),
                    response_id: Some("resp-background".to_string()),
                    token_usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        total_tokens: 120,
                        ..Default::default()
                    }),
                },
            )),
        ))]);
        let (sess, turn_context, rx) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = collect_compaction_output(
            &sess,
            &turn_context,
            stream,
            CompactionRequestKind::Background(&cancellation),
        )
        .await;

        assert!(matches!(result, Err(ref err)
            if matches!(err.details(), CodexErrorDetails::ResponseIncomplete(_))));
        assert_eq!(sess.token_usage_info().await, None);
        assert!(rx.is_empty());
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
        let output = collect_compaction_output(
            &sess,
            &turn_context,
            stream,
            CompactionRequestKind::Foreground,
        )
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
