//! Shared single-recipient preparation and admission; lifecycle aggregation belongs to callers.

use super::send_input::SendInputAdmissionStatus;
use super::send_input::SendInputMode;
use super::send_input::SendInputResult;
use super::*;
use crate::agent::agent_resolver::resolve_controlled_v1_agent_target;
use crate::agent::control::QueuedInputObservationParams;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::WakeEventMailboxSubscription;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use codex_thread_store::MailboxFinalSubscriptionRequest;
use std::sync::Arc;

pub(super) async fn resolve_receiver(
    session: &Arc<Session>,
    target: &str,
    mode: SendInputMode,
) -> Result<ThreadId, FunctionCallError> {
    if matches!(mode, SendInputMode::Mailbox(_))
        && let Ok(receiver) = ThreadId::from_string(target.strip_prefix("id:").unwrap_or(target))
    {
        // Mailbox identity resolution must not load or adopt a runtime.
        return Ok(receiver);
    }
    resolve_controlled_v1_agent_target(session, target).await
}

pub(super) struct PreparedReceiver {
    pub(super) agent: CollabAgentRef,
    sends_to_descendant: bool,
}

pub(super) async fn prepare_receiver(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    receiver: ThreadId,
    mode: SendInputMode,
    interrupt: bool,
) -> Result<PreparedReceiver, FunctionCallError> {
    if receiver == session.thread_id {
        return Err(FunctionCallError::RespondToModel(
            "an agent cannot send input to itself; continue the current turn directly".to_string(),
        ));
    }
    let control = &session.services.agent_control;
    let mailbox = matches!(mode, SendInputMode::Mailbox(_));
    if !mailbox
        && control.get_agent_metadata(receiver).is_some()
        && session.multi_agent_version() == Some(MultiAgentVersion::V2)
    {
        control
            .ensure_v2_agent_loaded(build_agent_resume_config(turn.as_ref())?, receiver)
            .await
            .map_err(|err| collab_agent_error(receiver, err))?;
    }
    let agent = control
        .get_agent_presentation_ref(receiver)
        .await
        .map_err(|err| collab_agent_error(receiver, err))?;
    let sends_to_descendant = !mailbox
        && control
            .is_live_agent_descendant(session.thread_id, receiver)
            .await
            .map_err(|err| collab_agent_error(receiver, err))?;
    if interrupt && !sends_to_descendant {
        return Err(FunctionCallError::RespondToModel(
            "an agent reply route authorizes input, not interruption of its parent or peer"
                .to_string(),
        ));
    }
    if interrupt {
        control
            .interrupt_agent(receiver)
            .await
            .map_err(|err| collab_agent_error(receiver, err))?;
    }
    Ok(PreparedReceiver {
        agent,
        sends_to_descendant,
    })
}

pub(super) struct RecipientInput<'a> {
    pub(super) receiver: &'a PreparedReceiver,
    pub(super) call_id: &'a str,
    pub(super) batch_id: Option<&'a str>,
    pub(super) items: Vec<UserInput>,
    pub(super) mode: SendInputMode,
}

pub(super) async fn admit_input(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    input: RecipientInput<'_>,
) -> Result<SendInputResult, FunctionCallError> {
    let RecipientInput {
        receiver,
        call_id,
        batch_id,
        items,
        mode,
    } = input;
    let receiver_id = receiver.agent.thread_id;
    let control = &session.services.agent_control;
    let response_observation = mode.response_observation();
    let start_options = crate::TurnStartOptions {
        parent_turn_id: Some(turn.sub_id.clone()),
        root_turn_id: turn.turn_metadata_state.root_turn_id(),
        cyber_access_program: turn.cyber_access_program,
        ..Default::default()
    };
    let result = async {
        if let SendInputMode::Mailbox(subscription) = mode {
            let subscription = match subscription {
                WakeEventMailboxSubscription::None => MailboxFinalSubscriptionRequest::None,
                WakeEventMailboxSubscription::Wake => MailboxFinalSubscriptionRequest::Wake,
            };
            control
                .accept_mailbox_agent_input_with_batch(
                    session.presentation_id(),
                    &turn.sub_id,
                    call_id,
                    receiver_id,
                    items,
                    subscription,
                    batch_id,
                )
                .await
                .map(|accepted| SendInputResult {
                    submission_id: accepted.id,
                    status: SendInputAdmissionStatus::MailboxAccepted,
                    hint: None,
                })
        } else if receiver.sends_to_descendant {
            let input = control
                .attribute_model_input(
                    session.presentation_id(),
                    receiver_id,
                    &turn.sub_id,
                    batch_id,
                    items,
                )
                .await?;
            if response_observation.queue_input() {
                control
                    .queue_input_observing_response(QueuedInputObservationParams {
                        agent_id: receiver_id,
                        input,
                        start_options,
                        observer: session.presentation_id(),
                        response_observation,
                        task_preview: None,
                        authored_selector: None,
                    })
                    .await
                    .map(|submission| SendInputResult {
                        submission_id: submission.queue_id.to_string(),
                        status: SendInputAdmissionStatus::Queued,
                        hint: None,
                    })
            } else {
                control
                    .send_agent_input_observing_response(
                        receiver_id,
                        input,
                        start_options,
                        session.presentation_id(),
                        response_observation,
                    )
                    .await
                    .map(|submission_id| SendInputResult {
                        submission_id,
                        status: SendInputAdmissionStatus::Submitted,
                        hint: None,
                    })
            }
        } else if response_observation.queue_input() {
            control
                .queue_scoped_agent_input_observing_response(
                    session.presentation_id(),
                    &turn.sub_id,
                    batch_id,
                    receiver_id,
                    items,
                    start_options,
                    response_observation,
                )
                .await
                .map(|submission| SendInputResult {
                    submission_id: submission.queue_id.to_string(),
                    status: SendInputAdmissionStatus::Queued,
                    hint: None,
                })
        } else {
            control
                .send_scoped_agent_input_observing_response(
                    session.presentation_id(),
                    &turn.sub_id,
                    batch_id,
                    receiver_id,
                    items,
                    start_options,
                    response_observation,
                )
                .await
                .map(|submission_id| SendInputResult {
                    submission_id,
                    status: SendInputAdmissionStatus::Submitted,
                    hint: None,
                })
        }
    }
    .await;
    result.map_err(|err| {
        if matches!(mode, SendInputMode::Mailbox(_))
            && let CodexErrorDetails::UnsupportedOperation(message) = err.details()
        {
            FunctionCallError::RespondToModel(message.clone())
        } else {
            collab_agent_error(receiver_id, err)
        }
    })
}
