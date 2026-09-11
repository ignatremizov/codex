use std::sync::Arc;

use super::RemoteCompactionV2Output;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact_remote::trim_function_call_history_to_fit_context_window;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::CompactionTraceContext;
use tracing::info;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
    pub(super) replacement_history_input: Vec<ResponseItemEnvelope>,
    pub(super) compacted_prefix_len: usize,
    pub(super) prompt_input: Vec<ResponseItem>,
    pub(super) prompt_input_metadata: Vec<Option<CodexHarnessMetadata>>,
    pub(super) compaction_output: ResponseItem,
    pub(super) compaction_response_id: String,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let media_sanitization = history.sanitize_compacted_media_prefix();
    if media_sanitization.changed() {
        info!(
            turn_id = %turn_context.sub_id,
            omitted_image_count = media_sanitization.omitted_image_count,
            omitted_inline_media_bytes = media_sanitization.omitted_inline_media_bytes,
            "removed previously compacted media before remote compaction v2"
        );
        analytics_details.omitted_image_count = Some(media_sanitization.omitted_image_count);
        analytics_details.omitted_inline_media_bytes =
            Some(media_sanitization.omitted_inline_media_bytes);
    }
    let base_instructions = sess.get_prompt_base_instructions().await;
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut history,
            turn_context.as_ref(),
            &base_instructions,
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction v2"
        );
    }
    if estimated_deleted_tokens > 0 {
        let max_local_deleted_tokens = sess
            .estimated_tokens_after_last_model_generated_item()
            .await;
        analytics_details.active_context_tokens_before = analytics_details
            .active_context_tokens_before
            .map(|active_context_tokens_before| {
                active_context_tokens_before
                    .saturating_sub(estimated_deleted_tokens.min(max_local_deleted_tokens))
            });
    }

    let compacted_prefix_len = history
        .compacted_prefix_len()
        .unwrap_or_default()
        .min(history.raw_items().len());
    let replacement_history_input = history.annotated_items().to_vec();
    let trace_input_history = compaction_trace
        .is_enabled()
        .then(|| history.raw_items().cloned().collect());
    let mut prompt_items =
        history.for_prompt_annotated(&turn_context.model_info().input_modalities);
    // Reconcile complete envelopes before separating item content from its metadata.
    sess.services
        .agent_control
        .messaging_context_snapshot(sess.presentation_id())
        .await?
        .reconcile_annotated(&mut prompt_items);
    if let Some(identities) = &step_context.agent_identities {
        crate::context::world_state::AgentIdentitiesState::new(identities)
            .reconcile_annotated(&mut prompt_items);
    }
    let (mut input, prompt_input_metadata): (Vec<_>, Vec<_>) = prompt_items
        .into_iter()
        .map(|envelope| (envelope.item, envelope.metadata))
        .unzip();
    if let Some(identities) = &step_context.agent_identities {
        crate::context::project_v1_agent_envelopes(&mut input, &identities.refs);
        crate::context::project_check_mail_results(
            &mut input,
            sess.thread_id,
            sess.services.thread_store.as_ref(),
            &identities.refs,
        )
        .await?;
        crate::context::project_mailbox_inventories(&mut input, sess.thread_id, &identities.refs);
    }
    let tool_router = &step_context.tool_router;
    input.push(ResponseItem::CompactionTrigger {});
    let prompt = Prompt {
        input,
        tools: tool_router.model_visible_specs(),
        parallel_tool_calls: true,
        base_instructions,
        output_schema: None,
        output_schema_strict: true,
        cyber_access_program: turn_context.cyber_access_program,
    };

    let responses_metadata = sess
        .responses_metadata(
            turn_context.as_ref(),
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        )
        .await;
    let trace_attempt = compaction_trace.start_attempt(&serde_json::json!({
        "model": turn_context.model_info().slug.as_str(),
        "instructions": prompt.base_instructions.text.as_str(),
        "input": &prompt.input,
        "parallel_tool_calls": prompt.parallel_tool_calls,
    }));
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        step_context,
        client_session,
        &prompt,
        &responses_metadata,
    )
    .await;
    trace_attempt.record_result(
        compaction_output_result
            .as_ref()
            .map(|output| std::slice::from_ref(&output.compaction_output)),
    );
    let RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage,
    } = compaction_output_result?;
    let mut prompt_input = prompt.input;
    prompt_input.pop();
    Ok(RemoteCompactV2Attempt {
        trace_input_history,
        replacement_history_input,
        compacted_prefix_len,
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        compaction_response_id: response_id,
        token_usage,
        owned_client_session,
    })
}
