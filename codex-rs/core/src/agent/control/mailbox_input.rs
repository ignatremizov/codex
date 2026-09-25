//! V1 mailbox acceptance, separate from target lifecycle and response observation.

use super::AgentControlInput;
use super::AgentModelInputOrigin;
use super::LocalAgentControl;
use super::SessionPresentationId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::MailboxFinalSubscriptionRequest;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::StoredMailboxInput;
use codex_thread_store::ThreadStore;
use std::sync::Arc;

impl LocalAgentControl {
    /// Accept user authorship from the client-facing Core boundary, never from agent input.
    ///
    /// An unloaded durable receiver retains pending mail until a compatible runtime resumes.
    /// User mail neither uses nor creates agent send grants. Its retry namespace is distinct
    /// from agent invocation keys, including when a client ID resembles an agent's entire key.
    pub(crate) async fn accept_mailbox_user_input(
        &self,
        store: Arc<dyn ThreadStore>,
        receiver: ThreadId,
        input: Vec<UserInput>,
        client_user_message_id: String,
    ) -> CodexResult<StoredMailboxInput> {
        let control = self.clone();
        tokio::spawn(async move {
            control
                .accept_mailbox_user_input_owned(
                    store.as_ref(),
                    receiver,
                    input,
                    client_user_message_id,
                )
                .await
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "mailbox acceptance worker lost: {error}; retry only the identical acceptance key"
            ))
        })?
    }

    async fn accept_mailbox_user_input_owned(
        &self,
        store: &dyn ThreadStore,
        receiver: ThreadId,
        input: Vec<UserInput>,
        client_user_message_id: String,
    ) -> CodexResult<StoredMailboxInput> {
        if client_user_message_id.is_empty() || input.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "user mailbox input requires input and a nonempty clientUserMessageId".to_string(),
            ));
        }
        let submission_key = serde_json::to_string(&("user-mailbox", &client_user_message_id))
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to encode user mailbox identity: {error}"))
            })?;
        let manager = self.upgrade()?;
        let recipient_lifecycle = manager.agent_lifecycle_lock(receiver).lock_owned().await;
        if let Some(existing) = store
            .lookup_mailbox_input(receiver, &submission_key)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to look up user mailbox acceptance: {error}"
                ))
            })?
        {
            if existing.receiver_thread_id != receiver
                || existing.submission_key != submission_key
                || existing.sender != MailboxSender::User
                || !matches!(
                    &existing.payload,
                    MailboxPayload::User { input: stored_input, client_id }
                        if stored_input == &input && client_id.as_deref() == Some(client_user_message_id.as_str())
                )
            {
                return Err(CodexErr::InvalidRequest(
                    "user mailbox client message ID already accepted different input or authorship"
                        .to_string(),
                ));
            }
            return Ok(existing);
        }
        if let Ok(target) = manager.get_thread(receiver).await {
            if target.config_snapshot().await.ephemeral {
                return Err(CodexErr::UnsupportedOperation(
                    "ephemeral receivers do not support durable user mailbox input".to_string(),
                ));
            }
            match target.effective_mailbox_multi_agent_version().await {
                MultiAgentVersion::V1 => {}
                MultiAgentVersion::V2 | MultiAgentVersion::Disabled => {
                    return Err(CodexErr::UnsupportedOperation(
                        "the loaded receiver's backend does not support mailbox consumption"
                            .to_string(),
                    ));
                }
            }
            // As with explicit thread placement, first input may precede materialization.
            // Persist the existing durable thread without starting a turn or pinning a backend.
            target
                .session
                .try_ensure_rollout_materialized(codex_thread_store::PersistContext::Standard)
                .await?;
            target.flush_rollout().await?;
        }
        // Archiving and unloading do not revoke the user's ability to leave durable notes.
        store
            .read_thread(ReadThreadParams {
                thread_id: receiver,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        // Without a loaded runtime, eligibility is deferred; do not reconstruct configuration,
        // guess a backend, or discard pending user input simply because the receiver is offline.
        let accepted = store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key,
                final_subscription: MailboxFinalSubscriptionRequest::None,
                payload: MailboxPayload::User {
                    input,
                    client_id: Some(client_user_message_id),
                },
            })
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to accept user mailbox input: {error}"))
            })?;
        drop(recipient_lifecycle);
        self.notify_mailbox_activity(receiver).await;
        Ok(accepted)
    }

    /// Accept immutable mail without loading, steering, or subscribing to its receiver.
    ///
    /// After persistence, a loaded receiver receives an activity hint for later inventory
    /// scheduling, not a payload-bearing turn or a delivery receipt. Retried invocations recover
    /// the original attribution even when aliases, runtime metadata, or send settings changed.
    #[cfg(test)]
    pub(crate) async fn accept_mailbox_agent_input(
        &self,
        sender: SessionPresentationId,
        sender_turn_id: &str,
        call_id: &str,
        receiver: ThreadId,
        input: Vec<UserInput>,
        final_subscription: MailboxFinalSubscriptionRequest,
    ) -> CodexResult<StoredMailboxInput> {
        self.accept_mailbox_agent_input_with_batch(
            AgentModelInputOrigin {
                sender,
                sender_turn_id: sender_turn_id.to_owned(),
            },
            call_id,
            receiver,
            input,
            final_subscription,
            /*batch_id*/ None,
        )
        .await
    }

    pub(crate) async fn accept_mailbox_agent_input_with_batch(
        &self,
        origin: AgentModelInputOrigin,
        call_id: &str,
        receiver: ThreadId,
        input: Vec<UserInput>,
        final_subscription: MailboxFinalSubscriptionRequest,
        batch_id: Option<&str>,
    ) -> CodexResult<StoredMailboxInput> {
        let control = self.clone();
        let call_id = call_id.to_string();
        let batch_id = batch_id.map(str::to_string);
        tokio::spawn(async move {
            control
                .accept_mailbox_agent_input_owned(
                    origin,
                    &call_id,
                    receiver,
                    input,
                    final_subscription,
                    batch_id.as_deref(),
                )
                .await
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "mailbox acceptance worker lost: {error}; retry only the identical acceptance key"
            ))
        })?
    }

    async fn accept_mailbox_agent_input_owned(
        &self,
        origin: AgentModelInputOrigin,
        call_id: &str,
        receiver: ThreadId,
        input: Vec<UserInput>,
        final_subscription: MailboxFinalSubscriptionRequest,
        batch_id: Option<&str>,
    ) -> CodexResult<StoredMailboxInput> {
        let AgentModelInputOrigin {
            sender,
            sender_turn_id,
        } = origin;
        let sender_turn_id = sender_turn_id.as_str();
        if sender.thread_id == receiver {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot send mailbox input to itself".to_string(),
            ));
        }
        if sender_turn_id.is_empty() || call_id.is_empty() || input.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "mailbox acceptance requires a sender turn, tool call identity, and input"
                    .to_string(),
            ));
        }
        // JSON tuple encoding avoids delimiter collisions in opaque turn/call IDs. Receiver
        // scoping is owned by the store's (receiver_thread_id, submission_key) unique key.
        let submission_key = serde_json::to_string(&(
            "v1-send-input-mailbox",
            sender.thread_id,
            sender_turn_id,
            call_id,
        ))
        .map_err(|error| {
            CodexErr::Fatal(format!("failed to encode mailbox invocation: {error}"))
        })?;
        let manager = self.upgrade()?;
        // Adoption and close serialize on this UUID even when no recipient runtime is loaded.
        // Take lifecycle before messaging, and retain it through attribution and persistence.
        // Unlike live-lifecycle admission, this lock neither loads nor validates a receiver:
        // accepted retries still return their stored outcome without requiring a fresh grant.
        let _recipient_lifecycle = manager.agent_lifecycle_lock(receiver).lock_owned().await;
        let sender_submission = self.state.mailbox_submission(sender.thread_id);
        let _destination = if final_subscription == MailboxFinalSubscriptionRequest::Wake {
            Some(
                Arc::clone(&sender_submission.semaphore)
                    .acquire_owned()
                    .await
                    .map_err(|error| CodexErr::Fatal(format!("mailbox sender closed: {error}")))?,
            )
        } else {
            None
        };
        let _permission = self.acquire_messaging_permission_transaction().await;
        let _observer = if final_subscription == MailboxFinalSubscriptionRequest::Wake {
            Some(self.acquire_response_observation_transaction(sender).await)
        } else {
            None
        };
        let source = manager.get_thread(sender.thread_id).await?;
        if source.session.presentation_id() != sender
            || !Arc::ptr_eq(&self.state, &source.session.services.agent_control.state)
            || source.session.active_agent_response_turn_id().as_deref() != Some(sender_turn_id)
        {
            return Err(CodexErr::InvalidRequest(
                "mailbox sender runtime or originating turn is no longer current".to_string(),
            ));
        }
        source.session.submission_admission.check_ready()?;
        let _sender_admission = source
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("mailbox sender is closing".into()))?;
        if source.multi_agent_version() != Some(MultiAgentVersion::V1) {
            return Err(CodexErr::UnsupportedOperation(
                "mailbox acceptance is only supported for V1 senders".to_string(),
            ));
        }
        let store = &source.session.services.thread_store;
        if let Some(existing) = store
            .lookup_mailbox_input(receiver, &submission_key)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("failed to look up mailbox acceptance: {error}"))
            })?
        {
            let matches = existing.receiver_thread_id == receiver
                && existing.submission_key == submission_key
                && existing.sender == MailboxSender::Agent(sender.thread_id)
                && existing.final_subscription.is_some()
                    == (final_subscription == MailboxFinalSubscriptionRequest::Wake)
                && matches!(
                    &existing.payload,
                    MailboxPayload::Agent { input: stored_input, attribution }
                        if stored_input == &input
                            && attribution.sender.thread_id == sender.thread_id
                            && attribution.recipient.thread_id == receiver
                            && attribution.sender_turn_id == sender_turn_id
                            && attribution.batch_id.as_deref() == batch_id
                );
            if !matches {
                return Err(CodexErr::InvalidRequest(
                    "mailbox invocation already accepted different input or authorship".to_string(),
                ));
            }
            // An accepted retry is not a new send. Do not recapture mutable attribution,
            // demand a new grant, or reset a consumed/rejected message to pending.
            if final_subscription == MailboxFinalSubscriptionRequest::Wake {
                // Recovery reads canonical retirement/delivery proof before acquiring its
                // observer transaction. The immutable acceptance row is not a fresh grant.
                self.schedule_mailbox_final_subscription_recovery(sender);
            }
            return Ok(existing);
        }

        let root = self.bound_session_id().ok_or_else(|| {
            CodexErr::UnsupportedOperation(
                "mailbox acceptance requires a configured same-root V1 identity".to_string(),
            )
        })?;
        let graph = manager
            .agent_graph_store()
            .filter(|graph| graph.supports_agent_aliases())
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation(
                    "mailbox acceptance requires durable agent identities".to_string(),
                )
            })?;
        for endpoint in [sender.thread_id, receiver] {
            let alias = graph
                .find_current_agent_alias_by_thread(endpoint)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to read mailbox endpoint identity: {error}"))
                })?;
            if !alias.is_some_and(|alias| alias.session_id == root) {
                return Err(CodexErr::UnsupportedOperation(
                    "mailbox acceptance currently supports only same-root agents; cross-root mail and implicit adoption are unsupported".to_string(),
                ));
            }
        }
        if !self
            .mailbox_send_permission_locked(sender.thread_id, receiver)
            .await?
        {
            return Err(CodexErr::UnsupportedOperation(
                "mailbox send requires configured directed/subtree permission or downward ancestry; transient m grants do not authorize mail".to_string(),
            ));
        }
        // Existence is a storage read, not a request to load or resume the receiver.
        if let Ok(target) = manager.get_thread(receiver).await {
            target
                .session
                .try_ensure_rollout_materialized(codex_thread_store::PersistContext::Standard)
                .await?;
            target.flush_rollout().await?;
        }
        store
            .read_thread(ReadThreadParams {
                thread_id: receiver,
                include_archived: false,
                include_history: false,
            })
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("mailbox receiver is unavailable: {error}"))
            })?;
        let AgentControlInput::AttributedAgentInput {
            attribution,
            presentation,
            ..
        } = self
            .attribute_model_input(sender, receiver, sender_turn_id, batch_id, input)
            .await?
        else {
            return Err(CodexErr::Fatal(
                "trusted mailbox attribution was not produced".to_string(),
            ));
        };
        #[cfg(test)]
        {
            let gate = self
                .wait_agent_presentations
                .mailbox_acceptance_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((reached, proceed)) = gate {
                let _ = reached.send(());
                let _ = proceed.await;
            }
        }
        let params = AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key,
            final_subscription,
            payload: MailboxPayload::Agent {
                input: presentation,
                attribution,
            },
        };
        let accepted = if final_subscription == MailboxFinalSubscriptionRequest::Wake {
            let accepted = self.accept_mailbox_subscription(&source, params).await?;
            if let Err(error) = self
                .project_accepted_mailbox_subscription(&source, &accepted)
                .await
            {
                source.session.quarantine_history(format!(
                    "accepted mailbox subscription projection failed: {error}"
                ));
                tracing::warn!(%error, "accepted subscription awaits durable recovery");
            }
            self.schedule_mailbox_final_subscription_recovery(sender);
            accepted
        } else {
            store.accept_mailbox_input(params).await.map_err(|error| {
                CodexErr::Fatal(format!("failed to accept mailbox input: {error}"))
            })?
        };
        drop(_observer);
        drop(_destination);
        drop(_permission);
        drop(_recipient_lifecycle);
        // Only fresh durable acceptance produces this best-effort live sibling notice.
        // Retries returned above; neither receipt failure nor a missing Main undoes acceptance.
        if let MailboxPayload::Agent { attribution, input } = &accepted.payload
            && let Some(id) = codex_protocol::mailbox_acceptance_receipt_id(&accepted.id)
        {
            let mut receipt = codex_protocol::items::AgentMessageItem::new(&[]);
            receipt.id = id.to_string();
            receipt.phase = Some(codex_protocol::models::MessagePhase::Commentary);
            receipt.attribution = Some(attribution.as_ref().clone());
            receipt.input = Some(input.clone());
            if let Err(error) = self
                .mirror_attributed_agent_input(
                    receiver,
                    &codex_protocol::items::TurnItem::AgentMessage(receipt),
                )
                .await
            {
                tracing::warn!(%error, "failed to present mailbox acceptance notice");
            }
        }
        // The existing scheduler owns idle inventory admission. A missing or concurrently
        // unloaded runtime cannot undo durable acceptance; the helper never loads a receiver.
        self.notify_mailbox_activity(receiver).await;
        Ok(accepted)
    }
}
