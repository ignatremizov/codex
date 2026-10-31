//! Compaction inside a regular turn, after its sampling request and tools have settled.
//! Explicit requests belong to that turn and coalesce with compaction already in progress.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use super::session::Session;
use super::step_context::StepContext;
use super::turn_context::TurnContext;
use crate::client::ModelClientSession;
use crate::compact_remote_v2::BackgroundCompactionCandidate;
use crate::tasks::emit_compact_metric;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_async_utils::OrCancelExt;
use codex_features::Feature;
use codex_model_provider::RemoteCompactionSupport;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
enum RequestState {
    #[default]
    Idle,
    Requested,
    Compacting,
}

#[derive(Default)]
struct CompactionRequest(Mutex<RequestState>);

/// Schedules only the originating turn; cancellation cannot compact a later turn.
pub(crate) fn request(turn_context: &TurnContext) {
    let request = turn_context
        .extension_data
        .get_or_init(CompactionRequest::default);
    let mut state = request.0.lock().unwrap_or_else(PoisonError::into_inner);
    *state = RequestState::Requested;
}

pub(crate) fn is_requested(turn_context: &TurnContext) -> bool {
    turn_context
        .extension_data
        .get::<CompactionRequest>()
        .is_some_and(|request| {
            matches!(
                *request.0.lock().unwrap_or_else(PoisonError::into_inner),
                RequestState::Requested
            )
        })
}

struct CompactionGuard {
    request: Arc<CompactionRequest>,
    installed: bool,
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        let mut state = self
            .request
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.installed || matches!(*state, RequestState::Compacting) {
            *state = RequestState::Idle;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct InlineCompactRun<'a> {
    pub(crate) reason: CompactionReason,
    pub(crate) phase: CompactionPhase,
    pub(crate) cancellation: &'a CancellationToken,
}

impl InlineCompactRun<'_> {
    pub(crate) fn trigger(self) -> CompactionTrigger {
        match self.reason {
            CompactionReason::UserRequested => CompactionTrigger::Manual,
            CompactionReason::ContextLimit
            | CompactionReason::ModelDownshift
            | CompactionReason::CompHashChanged => CompactionTrigger::Auto,
        }
    }
}

/// The replacement context may use a different model than the compactor.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "trace", skip_all, fields(reason = ?run.reason, phase = ?run.phase))]
pub(super) async fn run(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    replacement_step_context: Arc<StepContext>,
    client_session: &mut ModelClientSession,
    run: InlineCompactRun<'_>,
    candidate: Option<Box<BackgroundCompactionCandidate>>,
) -> CodexResult<bool> {
    if sess
        .services
        .thread_extension_data
        .get::<crate::codex_delegate::compaction::CompactionDecoder>()
        .is_some()
    {
        return Err(CodexErr::InvalidRequest(
            "Compaction decoders cannot compact their input".to_string(),
        ));
    }
    let phase = run.phase;
    let turn_context = &step_context.turn;
    if matches!(phase, CompactionPhase::PreTurn)
        && crate::guardian::is_basic_session_source(&turn_context.session_source)
        && !crate::guardian::should_compact_guardian_input(sess)?
    {
        return Ok(false);
    }
    let request = turn_context
        .extension_data
        .get_or_init(CompactionRequest::default);
    let requested = {
        let mut state = request.0.lock().unwrap_or_else(PoisonError::into_inner);
        let requested = matches!(*state, RequestState::Requested);
        *state = RequestState::Compacting;
        requested
    };
    let mut request_guard = CompactionGuard {
        request,
        installed: false,
    };
    // Admission can race the caller's checkpoint. An explicit request must compact the live
    // history rather than install or skip a speculative prefix prepared before that request.
    let run = if requested {
        InlineCompactRun {
            reason: CompactionReason::UserRequested,
            ..run
        }
    } else {
        run
    };
    let candidate = if requested { None } else { candidate };
    let reason = run.reason;
    let world_state = Arc::new(
        sess.build_world_state_for_step(&replacement_step_context, /*new_window*/ true)
            .await?,
    );
    let _profile_guard = turn_context.turn_timing_state.begin_compaction();
    let _compaction_span = tracing::trace_span!(
        "codex.compaction",
        codex.turn.phase = "compaction",
        conversation.id = %sess.thread_id,
        turn.id = %turn_context.sub_id,
    );
    let trigger = run.trigger();
    let manual = matches!(trigger, CompactionTrigger::Manual);
    if turn_context.config.features.enabled(Feature::TokenBudget) {
        crate::compact_token_budget::run_inline_auto_compact_task(
            Arc::clone(sess),
            replacement_step_context,
            world_state,
            trigger,
        )
        .await?;
        request_guard.installed = true;
        return Ok(true);
    }

    let remote_compaction = if turn_context
        .config
        .features
        .enabled(Feature::RemoteCompaction)
    {
        turn_context.provider.capabilities().remote_compaction
    } else {
        RemoteCompactionSupport::Unsupported
    };
    let result = match remote_compaction {
        RemoteCompactionSupport::V2 => {
            let has_candidate = candidate.is_some();
            if !has_candidate {
                emit_compact_metric(&sess.services.session_telemetry, "remote_v2", manual);
            }
            let fallback = if has_candidate && matches!(reason, CompactionReason::ContextLimit) {
                let status =
                    super::context_window::context_window_token_status(sess, turn_context).await;
                let required = match phase {
                    CompactionPhase::PostTurn => status.turn_end_compaction_threshold_reached,
                    CompactionPhase::PreTurn
                    | CompactionPhase::MidTurn
                    | CompactionPhase::StandaloneTurn => status.token_limit_reached,
                };
                if required {
                    crate::compact_remote_v2::CompactionFallback::Foreground
                } else {
                    crate::compact_remote_v2::CompactionFallback::Skip
                }
            } else {
                crate::compact_remote_v2::CompactionFallback::Foreground
            };
            let installed = crate::compact_remote_v2::run_inline_remote_auto_compact_task(
                Arc::clone(sess),
                step_context,
                replacement_step_context,
                client_session,
                world_state,
                run,
                candidate,
                fallback,
            )
            .await?;
            if has_candidate && installed {
                emit_compact_metric(&sess.services.session_telemetry, "remote_v2", manual);
            }
            Ok(installed)
        }
        RemoteCompactionSupport::Unsupported => {
            emit_compact_metric(&sess.services.session_telemetry, "local", manual);
            crate::compact::run_inline_auto_compact_task(
                Arc::clone(sess),
                Arc::clone(turn_context),
                replacement_step_context,
                world_state,
                run,
            )
            .or_cancel(run.cancellation)
            .await??;
            Ok(true)
        }
    };
    request_guard.installed = result.as_ref().is_ok_and(|installed| *installed);
    result
}
