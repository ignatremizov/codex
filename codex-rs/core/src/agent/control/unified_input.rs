//! Selected-controller admission for trusted structured agent input.
//!
//! This is the native prerequisite for a unified V2 tool, not a public tool switch.
//! Encrypted/audited messages and durable mailbox input need their own exact typed
//! admission and storage implementation. Reject these capabilities before effects;
//! never relabel encrypted content as human user input or bypass a host controller.

use super::*;
use crate::agent::api::UnifiedInputPolicy;
use crate::agent::api::UnifiedInputReceipt;
use crate::agent::api::UnifiedInputStatus;
use crate::agent::api::UnifiedModelInput;
use crate::agent::api::UnifiedModelInputRequest;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::agent::user_control::UserAgentInputOutcome;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentResponseFinalDelivery;

impl LocalAgentControl {
    /// Reuse exact sender/recipient validation, FIFO, and observer transactions.
    ///
    /// Prepared encrypted/audited content is deliberately not accepted by this
    /// structured-input path. `submit_observed_turn_input` cannot obtain an exact
    /// receiving-turn receipt for the old InterAgentCommunication/Accepted route.
    pub(crate) async fn submit_native_unified_input(
        &self,
        request: UnifiedModelInputRequest,
    ) -> CodexResult<UnifiedInputReceipt> {
        let manager = self.runtime.upgrade()?;
        manager.ensure_native_agent_control()?;
        let UnifiedModelInputRequest {
            caller,
            source_instance_id,
            sender_turn_id,
            call_id,
            batch_id,
            target,
            resume_config,
            input,
            policy,
            interrupt_descendant,
            start_options,
        } = request;
        let items = match input {
            UnifiedModelInput::Structured(items) if !items.is_empty() => items,
            UnifiedModelInput::Structured(_) => {
                return Err(CodexErr::InvalidRequest(
                    "unified agent input must contain at least one structured item".into(),
                ));
            }
            UnifiedModelInput::Prepared(_) => {
                return Err(CodexErr::UnsupportedOperation(
                    "unified encrypted/audited agent input requires typed receiving-turn admission"
                        .into(),
                ));
            }
        };
        if matches!(policy, UnifiedInputPolicy::Mailbox { .. }) {
            return Err(CodexErr::UnsupportedOperation(
                "unified V2 durable mailbox input needs versioned payload/claim semantics".into(),
            ));
        }
        if call_id.is_empty()
            || sender_turn_id.is_empty()
            || batch_id.as_deref().is_some_and(str::is_empty)
            || start_options.parent_turn_id.as_deref() != Some(sender_turn_id.as_str())
        {
            return Err(CodexErr::InvalidRequest(
                "unified input needs the exact sender turn, tool call, and parent-turn identity"
                    .into(),
            ));
        }
        let sender = SessionPresentationId::new(caller, source_instance_id);
        let source = manager.get_thread(caller).await?;
        if source.session.presentation_id() != sender
            || !self.owns_messaging_runtime(&source)
            || source.session.active_agent_response_turn_id().as_deref()
                != Some(sender_turn_id.as_str())
        {
            return Err(CodexErr::InvalidRequest(
                "unified agent input source runtime or originating turn is no longer current"
                    .into(),
            ));
        }
        // Resolving native selectors happens only after exact caller validation.
        let receiver_id = self.resolve_target(caller, &target).await?;
        if receiver_id == caller {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot submit unified input to itself".into(),
            ));
        }
        let _residency_pin = if self.get_agent_metadata(receiver_id).is_some()
            && source.multi_agent_version() == Some(MultiAgentVersion::V2)
        {
            self.ensure_v2_agent_loaded(resume_config, receiver_id, /*parent*/ None)
                .await?
        } else {
            None
        };
        let receiver = manager.get_thread(receiver_id).await?;
        if !self.owns_messaging_runtime(&receiver) {
            return Err(CodexErr::InvalidRequest(
                "unified agent input receiver belongs to another runtime".into(),
            ));
        }
        let response_observation = match policy {
            UnifiedInputPolicy::Default => {
                let final_response = if self.is_live_agent_descendant(receiver_id, caller).await? {
                    FinalResponseObservation::None
                } else {
                    FinalResponseObservation::Passive
                };
                ResponseObservationPolicy::from_parts(/*commentary*/ false, final_response)
            }
            UnifiedInputPolicy::Response {
                commentary,
                final_delivery,
            }
            | UnifiedInputPolicy::Queue {
                commentary,
                final_delivery,
            } => {
                let final_response = match final_delivery {
                    AgentResponseFinalDelivery::None => FinalResponseObservation::None,
                    AgentResponseFinalDelivery::PresentationOnly => {
                        FinalResponseObservation::PresentationOnly
                    }
                    AgentResponseFinalDelivery::Passive => FinalResponseObservation::Passive,
                    AgentResponseFinalDelivery::Wake => FinalResponseObservation::Wake,
                };
                ResponseObservationPolicy::from_turn_parts(
                    commentary,
                    final_response,
                    matches!(policy, UnifiedInputPolicy::Queue { .. }),
                )
            }
            UnifiedInputPolicy::Mailbox { .. } => {
                unreachable!("durable V2 mailbox was rejected before target resolution")
            }
        };
        if interrupt_descendant && !self.is_live_agent_descendant(caller, receiver_id).await? {
            return Err(CodexErr::InvalidRequest(
                "an agent can interrupt only its descendants, not parents or peers".into(),
            ));
        }
        // Capture true agent authorship and original typed presentation before
        // any interrupt or delivery. The existing admission owner rechecks epochs.
        let attributed = self
            .attribute_model_input(
                sender,
                receiver_id,
                &sender_turn_id,
                batch_id.as_deref(),
                items,
            )
            .await?;
        if interrupt_descendant {
            self.interrupt_spawned_agent(caller, receiver_id).await?;
        }
        if response_observation.queue_input() {
            let queued = self
                .queue_input_observing_response(QueuedInputObservationParams {
                    agent_id: receiver_id,
                    input: attributed,
                    start_options,
                    observer: sender,
                    response_observation,
                    task_preview: None,
                    authored_selector: None,
                })
                .await?;
            return Ok(UnifiedInputReceipt {
                submission_id: queued.queue_id.to_string(),
                status: UnifiedInputStatus::Queued,
                target_turn_id: None,
                warning: None,
            });
        }
        let admission = self
            .send_agent_input_observing_response(
                receiver_id,
                attributed,
                start_options,
                sender,
                response_observation,
            )
            .await?;
        let status = match (
            admission.input_outcome,
            admission.post_admission_warning.as_ref(),
        ) {
            (UserAgentInputOutcome::Unknown, _) => UnifiedInputStatus::Unknown,
            (UserAgentInputOutcome::Admitted, Some(_)) => UnifiedInputStatus::AdmittedWithWarning,
            (UserAgentInputOutcome::Admitted, None) => UnifiedInputStatus::Submitted,
            (UserAgentInputOutcome::Queued, _) => UnifiedInputStatus::Queued,
        };
        Ok(UnifiedInputReceipt {
            submission_id: admission.submission_id,
            status,
            target_turn_id: admission.target_turn_id,
            warning: admission.post_admission_warning,
        })
    }
}
