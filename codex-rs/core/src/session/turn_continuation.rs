//! Explicit continuation uses the original turn's authority, settings, and canonical results.
//! The command is not user input. Interrupt notices are omitted only after the context edit
//! has been durably published; tool results and the original abort event remain untouched.

use super::Session;
use codex_history::RolloutItem;
use codex_history::TurnRecoveryMode;
use codex_history::select_recoverable_turn;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnContextItem;
use codex_thread_store::LoadThreadHistoryParams;
use codex_utils_path_uri::PathUri;

/// Model-invisible context edits carried through subsequent steps and recovery of this turn.
pub(crate) struct ClearedInterrupts(pub(crate) Vec<String>);

/// Commit the omission before any startup contributor, compactor, or model can see history.
pub(super) async fn record_context_edit(
    session: &Session,
    original: &TurnContextItem,
    cleared: &[String],
) -> CodexResult<()> {
    let mut context = original.clone();
    let saved = context.recovery_start_options.as_mut().ok_or_else(|| {
        CodexErr::InvalidRequest("The original turn settings were not recorded.".to_string())
    })?;
    if saved.cleared_interrupt_message_ids == cleared {
        return Ok(());
    }
    saved.cleared_interrupt_message_ids = cleared.to_vec();
    let permit = session.acquire_history_publication_barrier().await?;
    if session.reference_context_item().await.as_ref() != Some(original) {
        return Err(CodexErr::InvalidRequest(
            "The turn context changed before continuation was admitted.".to_string(),
        ));
    }
    let receipt = session.dispatch_history_publication(
        permit,
        vec![RolloutItem::TurnContext(context.clone())],
        Vec::new(),
        /*acknowledgement*/ None,
        move |state| {
            state.history.apply_turn_continuation(&context);
            state.set_reference_context_item(Some(context));
        },
    )?;
    session.publication_result(receipt).await
}

pub(super) async fn prepare(session: &Session) -> CodexResult<TurnContextItem> {
    let _permit = session.acquire_history_publication_barrier().await?;
    session
        .services
        .thread_store
        .flush_thread(session.thread_id())
        .await
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    let segments = session
        .services
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        })
        .await
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    let own = segments.segments.last().ok_or_else(|| {
        CodexErr::InvalidRequest("There is no recorded turn to continue.".to_string())
    })?;
    let context = select_recoverable_turn(session.thread_id(), own, TurnRecoveryMode::Explicit)
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?
        .ok_or_else(|| {
            CodexErr::InvalidRequest(
                "There is no interrupted or unfinished regular turn to continue.".to_string(),
            )
        })?;
    if session.reference_context_item().await.as_ref() != Some(&context) {
        return Err(CodexErr::InvalidRequest(
            "The original turn context is no longer current; submit new input instead.".to_string(),
        ));
    }
    let config = session.thread_config_snapshot().await;
    let saved = context.recovery_start_options.as_ref().ok_or_else(|| {
        CodexErr::InvalidRequest("The original turn settings were not recorded.".to_string())
    })?;
    if context.model != config.model
        || context.collaboration_mode.as_ref() != Some(&config.collaboration_mode)
        || context.effort != config.reasoning_effort
        || saved.personality != config.personality
        || context.disabled_plugin_ids.as_ref() != Some(&config.disabled_plugin_ids)
        // Core resolves an unset selection and checks the effective summary before admission.
        || (config.reasoning_summary.is_some()
            && config.reasoning_summary != saved.configured_reasoning_summary
            && config.reasoning_summary != saved.reasoning_summary.or(context.summary))
        || (saved.service_tier.is_none() && config.service_tier.is_some())
    {
        return Err(CodexErr::InvalidRequest(
            "Turn settings changed since the interruption; submit new input instead.".to_string(),
        ));
    }
    // The persisted context currently certifies one thread-configured local environment only.
    // Do not silently reinterpret remote identities or permissions after a harness restart.
    let [environment] = config.environment_selections() else {
        return Err(CodexErr::InvalidRequest(
            "The original execution environment cannot be reproduced.".to_string(),
        ));
    };
    if environment.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID
        || environment.config != EnvironmentConfigState::FromThread
        || environment.cwd != PathUri::from_abs_path(&context.cwd)
        || context.cwd != *config.cwd()
        || context.permission_profile() != config.permission_profile
        || context.approval_policy != config.approval_policy
        || context.approvals_reviewer != Some(config.approvals_reviewer)
        || saved.workspace_roots.as_deref() != Some(config.effective_workspace_roots())
    {
        return Err(CodexErr::InvalidRequest(
            "Permissions or execution environment changed; submit new input instead.".to_string(),
        ));
    }
    Ok(context)
}
