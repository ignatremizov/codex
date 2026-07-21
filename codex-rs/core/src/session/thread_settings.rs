//! Handles persistent thread-settings updates and serializes their persistence
//! with checkpoints written directly to storage.

use super::durable_context::PublicationBatch;
use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::StepSettingsUpdate;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_thread_store::ThreadStoreResult;
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

impl Session {
    /// Captures and flushes current settings under the shared persistence permit.
    pub(crate) async fn checkpoint_thread_settings(&self) -> ThreadStoreResult<()> {
        let permit = acquire_persistence_lock(self).await;
        let result = match self.dispatch_history_publication(
            permit,
            vec![RolloutItem::EventMsg(applied_event(self).await)],
            Vec::new(),
            /*acknowledgement*/ None,
            |_| (),
        ) {
            Ok(receiver) => self.publication_result(receiver).await,
            Err(error) => Err(error),
        };
        result.map_err(|error| codex_thread_store::ThreadStoreError::Internal {
            message: error.to_string(),
        })
    }
}

/// Applies standalone thread settings and reports invalid overrides through the
/// normal event stream.
pub(super) async fn update(
    session: &Arc<Session>,
    submission_id: String,
    overrides: ThreadSettingsOverrides,
) {
    let updates = prepare_update(overrides);
    if let Err(error) = apply_update(session, submission_id.clone(), updates).await {
        session
            .send_event_raw(Event {
                id: submission_id,
                msg: EventMsg::Error(error.to_error_event(/*message_prefix*/ None)),
            })
            .await;
    } else {
        // Standalone settings changes supersede a pending automatic continuation.
        session.state.lock().await.last_started_turn_id = None;
    }
}

/// Converts protocol overrides into the internal settings update shape.
pub(super) fn prepare_update(overrides: ThreadSettingsOverrides) -> SessionSettingsUpdate {
    let ThreadSettingsOverrides {
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        model,
        effort,
        summary,
        service_tier,
        collaboration_mode,
        personality,
        disabled_plugin_ids,
    } = overrides;
    SessionSettingsUpdate {
        step_settings: StepSettingsUpdate {
            model,
            effort,
            collaboration_mode,
            reasoning_summary: summary,
            service_tier,
            personality,
            approval_policy,
            approvals_reviewer,
        },
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        disabled_plugin_ids,
        ..Default::default()
    }
}

/// Acquires the shared permit before capturing or changing persistent settings.
pub(super) async fn acquire_persistence_lock(session: &Session) -> OwnedSemaphorePermit {
    session
        .thread_settings_persistence
        .clone()
        .acquire_owned()
        .await
        .unwrap_or_else(|_| unreachable!("thread settings persistence semaphore is never closed"))
}

/// Applies persistent settings and emits the resulting thread-owned snapshot.
pub(super) async fn apply_update(
    session: &Session,
    submission_id: String,
    updates: SessionSettingsUpdate,
) -> CodexResult<()> {
    let settings_guard = acquire_persistence_lock(session).await;
    session.check_history_publication()?;
    let Some(commit) = session
        .update_settings_if_with_permit(updates, |_, _| true, &settings_guard)
        .await
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?
    else {
        unreachable!("unconditional settings update");
    };
    emit_applied(session, settings_guard, submission_id, commit.snapshot).await
}

/// Emits the snapshot published by one successful settings update.
pub(super) async fn emit_applied(
    session: &Session,
    permit: OwnedSemaphorePermit,
    submission_id: String,
    snapshot: ThreadSettingsSnapshot,
) -> CodexResult<()> {
    let msg = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: snapshot,
    });
    let persist = match session.current_rollout_path().await {
        Ok(Some(path)) => codex_rollout::existing_rollout_path(&path).await.is_some(),
        Ok(None) => true,
        Err(error) => {
            tracing::warn!("failed to check settings persistence path: {error}");
            true
        }
    };
    let items = if persist {
        vec![RolloutItem::EventMsg(msg.clone())]
    } else {
        Vec::new()
    };
    let receiver = session.dispatch_history_publication_with_events(
        permit,
        PublicationBatch {
            raw_event_guardian_thread_id: None,
            rollout: items,
            events: vec![Event {
                id: submission_id,
                msg,
            }],
        },
        Vec::new(),
        /*acknowledgement*/ None,
        |_| (),
    )?;
    session.publication_result(receiver).await
}

/// Builds a current thread-owned snapshot for storage checkpoints.
pub(super) async fn applied_event(session: &Session) -> EventMsg {
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: session.thread_settings_snapshot().await,
    })
}
