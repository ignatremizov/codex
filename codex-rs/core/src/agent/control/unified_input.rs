//! Selected-controller admission for trusted structured and prepared agent input.
//!
//! This is the native prerequisite for a unified V2 tool, not a public tool switch.
//! Prepared V2 input has an exact typed admission path, separate from legacy mail.
//! Durable V2 mailbox input still requires versioned claims and consumption; reject
//! that unsupported capability before effects and never bypass a host controller.

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
    /// Prepared encrypted/audited content uses its own trusted turn-input variant.
    /// The old InterAgentCommunication/Accepted route is never relabeled as exact
    /// turn admission, and the selected host backend is never bypassed.
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
            mut start_options,
        } = request;
        if matches!(&input, UnifiedModelInput::Structured(items) if items.is_empty()) {
            return Err(CodexErr::InvalidRequest(
                "unified agent input must contain at least one structured item".into(),
            ));
        }
        if matches!(policy, UnifiedInputPolicy::Mailbox { .. }) {
            return Err(CodexErr::UnsupportedOperation(
                "unified V2 durable mailbox input needs versioned payload/claim semantics".into(),
            ));
        }
        if let UnifiedModelInput::Prepared(message) = &input {
            let invalid = match message {
                crate::agent::types::AgentMessage::Plaintext(content)
                | crate::agent::types::AgentMessage::Encrypted(content) => {
                    content.trim().is_empty()
                }
                crate::agent::types::AgentMessage::EncryptedWithAudit {
                    encrypted_content,
                    audit_content,
                } => encrypted_content.trim().is_empty() || audit_content.trim().is_empty(),
            };
            if invalid {
                return Err(CodexErr::InvalidRequest(
                    "prepared agent input requires nonempty payload and any required readable audit".into(),
                ));
            }
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
        if matches!(&input, UnifiedModelInput::Prepared(_))
            && source.multi_agent_version() != Some(MultiAgentVersion::V2)
        {
            return Err(CodexErr::UnsupportedOperation(
                "unified encrypted/audited agent input requires typed receiving-turn admission on a native V2 backend".into(),
            ));
        }
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
        if matches!(&input, UnifiedModelInput::Prepared(_))
            && receiver.multi_agent_version() != Some(MultiAgentVersion::V2)
        {
            return Err(CodexErr::UnsupportedOperation(
                "prepared agent input requires a receiver already pinned to native V2".into(),
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
        let attributed = match input {
            UnifiedModelInput::Structured(items) => {
                self.attribute_model_input(
                    sender,
                    receiver_id,
                    &sender_turn_id,
                    batch_id.as_deref(),
                    items,
                )
                .await?
            }
            UnifiedModelInput::Prepared(message) => {
                // Match structured input's live endpoint check before capturing
                // provenance or performing an interruption. SessionIo rechecks
                // both incarnations at actual admission, including queue drain.
                self.ensure_model_input_authorized(sender, receiver.session.presentation_id())
                    .await?;
                let author = self
                    .get_agent_metadata(caller)
                    .and_then(|metadata| metadata.agent_path)
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "prepared input source has no native agent path".into(),
                        )
                    })?;
                let recipient = self
                    .get_agent_metadata(receiver_id)
                    .and_then(|metadata| metadata.agent_path)
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "prepared input receiver has no native agent path".into(),
                        )
                    })?;
                if start_options
                    .initiating_agent_path
                    .as_ref()
                    .is_some_and(|path| path != &author)
                {
                    return Err(CodexErr::InvalidRequest(
                        "prepared input initiating path differs from its captured author".into(),
                    ));
                }
                start_options.initiating_agent_path = Some(author.clone());
                let (_, attribution) = self
                    .presentation_attribution(
                        caller,
                        receiver_id,
                        &sender_turn_id,
                        batch_id.as_deref(),
                    )
                    .await?;
                let prepared = codex_protocol::PreparedAgentInput {
                    communication: message.into_communication(
                        author,
                        recipient,
                        crate::agent::types::MessageDeliveryMode::TriggerTurn,
                    ),
                    attribution: Box::new(attribution),
                    sender_instance_id: source_instance_id,
                    recipient_instance_id: receiver.session.presentation_id().instance_id,
                    call_id,
                };
                prepared
                    .validate_receiver(receiver_id)
                    .map_err(|message| CodexErr::InvalidRequest(message.to_owned()))?;
                AgentControlInput::PreparedAgentInput(Box::new(prepared))
            }
        };
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
