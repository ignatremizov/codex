//! Inventory admission and canonical recording, separate from payload consumption.

use super::input_queue::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::codex_thread::CodexThread;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_thread_store::MailboxInventoryNotification;
use codex_thread_store::MailboxInventoryRecovery;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreResult;
use std::sync::Arc;

#[cfg(test)]
#[path = "mailbox_inventory_tests.rs"]
mod tests;

pub(super) enum AutomaticIdleAdmission {
    Input(Vec<TurnInput>),
    Inventory(MailboxInventoryNotification),
}

pub(super) enum InventoryRecording {
    Recorded,
    NotNeeded,
    Obsolete,
}

/// Result of one serialized inventory idle-admission attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxInventoryAdmission {
    /// A stale unrecorded snapshot was cancelled and newer pending mail exists.
    Retry,
    /// No turn was admitted; do not immediately retry this outcome.
    Deferred,
    /// This admission recorded its inventory and started the reserved turn.
    Started,
}

impl CodexThread {
    /// Previews current mailbox eligibility without pinning the thread backend.
    pub async fn effective_mailbox_multi_agent_version(&self) -> MultiAgentVersion {
        self.session.effective_mailbox_multi_agent_version().await
    }

    /// Lowest-priority idle admission. The queue dispatcher must supply its lease
    /// after checking that no queued user input remains.
    pub async fn try_start_mailbox_inventory_if_idle_with_lease(
        &self,
        dispatch_lease: impl Send + 'static,
    ) -> CodexResult<MailboxInventoryAdmission> {
        if self.effective_mailbox_multi_agent_version().await != MultiAgentVersion::V1 {
            return Ok(MailboxInventoryAdmission::Deferred);
        }
        let control = &self.session.services.agent_control;
        control
            .ensure_execution_capacity_for_turn_start(self)
            .await?;
        let Some(lifecycle_lease) = control
            .mailbox_inventory_lifecycle_lease(self.session.thread_id)
            .await?
        else {
            return Ok(MailboxInventoryAdmission::Deferred);
        };
        // Goals run before this contributor and may have started a turn.
        if self.session.active_turn.lock().await.is_some() {
            return Ok(MailboxInventoryAdmission::Deferred);
        }
        let notification = self
            .session
            .prepare_idle_mailbox_inventory()
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("mailbox inventory preparation failed: {error}"))
            })?;
        let Some(notification) = notification else {
            return Ok(MailboxInventoryAdmission::Deferred);
        };
        // Only admitted work gets a task, while the queue/lifecycle leases are
        // still held. This keeps placeholder cleanup and ambiguous canonical
        // writes alive if the requesting RPC is cancelled. Concurrent hints
        // cannot create more tasks once this receiver has an active placeholder.
        let session = Arc::clone(&self.session);
        let admission = tokio::spawn(async move {
            session
                .try_start_automatic_idle_with_lease(
                    AutomaticIdleAdmission::Inventory(notification),
                    (dispatch_lease, lifecycle_lease),
                )
                .await
        });
        match admission.await.map_err(|error| {
            CodexErr::Fatal(format!("mailbox inventory admission task failed: {error}"))
        })? {
            Ok(outcome) => Ok(outcome),
            Err(error)
                if error.reason() == TryStartTurnIfIdleRejectionReason::PersistenceFailed =>
            {
                Err(CodexErr::Fatal(
                    "mailbox inventory could not be durably recorded".to_string(),
                ))
            }
            Err(_) => Ok(MailboxInventoryAdmission::Deferred),
        }
    }
}

impl Session {
    pub(crate) async fn effective_mailbox_multi_agent_version(&self) -> MultiAgentVersion {
        let configuration = self.state.lock().await.session_configuration.clone();
        let config = self.build_per_turn_config(&configuration, configuration.cwd().clone());
        let model_info = configuration
            .step_settings
            .resolve_model_info(
                self.services.models_manager.as_ref(),
                &configuration.model_info_overrides,
                self.features.enabled(Feature::Personality),
            )
            .await;
        self.preview_multi_agent_version_for_model(&model_info, &config)
    }

    async fn prepare_idle_mailbox_inventory(
        &self,
    ) -> ThreadStoreResult<Option<MailboxInventoryNotification>> {
        let _durable = self.durable_context_lock.acquire().await.map_err(|error| {
            ThreadStoreError::Internal {
                message: format!("failed to lock inventory context: {error}"),
            }
        })?;
        let Some(live) = self.live_thread() else {
            return Ok(None);
        };
        live.flush_canonical().await?;
        let store = &self.services.thread_store;
        let inventory = store.read_mailbox_inventory(self.thread_id).await?;
        if let Some(notification) = inventory.active_notification {
            match store
                .recover_mailbox_inventory(notification.clone())
                .await?
            {
                MailboxInventoryRecovery::AlreadyCovered { .. } => return Ok(None),
                MailboxInventoryRecovery::Recorded { .. } => {
                    store.reconcile_mailbox_inventory(notification).await?;
                }
                MailboxInventoryRecovery::NotRecorded => {
                    if store
                        .has_pending_mailbox_inventory(notification.clone())
                        .await?
                    {
                        return Ok(Some(notification));
                    }
                    store.cancel_mailbox_inventory(notification).await?;
                }
            }
        }
        store.prepare_mailbox_inventory(self.thread_id).await
    }

    /// Completes canonical proof and watermark before any model sampling.
    /// The caller already owns the idle placeholder, but no scheduler lease.
    pub(super) async fn record_idle_mailbox_inventory(
        &self,
        turn: &TurnContext,
        notification: MailboxInventoryNotification,
    ) -> ThreadStoreResult<InventoryRecording> {
        let _durable = self.durable_context_lock.acquire().await.map_err(|error| {
            ThreadStoreError::Internal {
                message: format!("failed to lock inventory recording: {error}"),
            }
        })?;
        let live = self
            .live_thread()
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: "inventory requires canonical thread persistence".to_string(),
            })?;
        live.flush_canonical().await?;
        let store = &self.services.thread_store;
        let context = match store
            .recover_mailbox_inventory(notification.clone())
            .await?
        {
            MailboxInventoryRecovery::AlreadyCovered { .. } => {
                return Ok(InventoryRecording::NotNeeded);
            }
            MailboxInventoryRecovery::Recorded { .. } => {
                store.reconcile_mailbox_inventory(notification).await?;
                return Ok(InventoryRecording::NotNeeded);
            }
            MailboxInventoryRecovery::NotRecorded => {
                if !store
                    .has_pending_mailbox_inventory(notification.clone())
                    .await?
                {
                    store.cancel_mailbox_inventory(notification).await?;
                    return Ok(InventoryRecording::Obsolete);
                }
                let context = notification.context()?;
                live.append_items_and_flush_canonical(&[RolloutItem::ResponseItem(
                    context.clone(),
                )])
                .await?;
                context
            }
        };
        {
            let mut state = self.state.lock().await;
            if !state
                .history
                .raw_items()
                .any(|item| item.id() == context.id())
            {
                state
                    .current_time_reminder
                    .note_recorded_items(std::slice::from_ref(&context.item));
                state.history.record_annotated_items(
                    std::slice::from_ref(&context),
                    turn.model_info().truncation_policy.into(),
                );
            }
        }
        store.reconcile_mailbox_inventory(notification).await?;
        Ok(InventoryRecording::Recorded)
    }
}
