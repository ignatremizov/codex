//! Typed routing of observed input without weakening the strict model-tool adapter.

use super::Session;
use super::SessionIo;
use super::new_submission_id;
use super::response_observation::AgentQueueTurnStartPermit;
use super::response_observation::InputTurnAdmissionResolution;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use codex_protocol::turn_input::NotSubmittedReason;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use tokio::sync::oneshot;

#[cfg(test)]
#[path = "observed_input_tests.rs"]
mod tests;

pub(crate) enum ObservedTurnInputSubmission {
    Admitted {
        submission_id: String,
        resolution: InputTurnAdmissionResolution,
        queue_start_permit: Option<AgentQueueTurnStartPermit>,
        input_persisted: Option<oneshot::Receiver<CodexResult<()>>>,
    },
    NotSubmitted {
        reason: NotSubmittedReason,
    },
    AdmittedWithoutObservation {
        submission_id: String,
        target_turn_id: String,
        warning: String,
    },
    Indeterminate {
        submission_id: String,
        warning: String,
    },
}

impl SessionIo {
    /// Returns the routing result and, only for accepted work, the exact response cursor.
    ///
    /// The mode travels with the queued operation. Dropping this result waiter cannot change an
    /// idle-only request into a steer or retract input already accepted by Core.
    pub(crate) async fn submit_observed_turn_input(
        &self,
        session: &Session,
        request: TurnInputRequest,
        mode: TurnInputMode,
    ) -> CodexResult<ObservedTurnInputSubmission> {
        self.submit_observed_input(session, request, mode, /*queue_metadata*/ None)
            .await
    }

    pub(crate) async fn submit_observed_queued_turn_input(
        &self,
        session: &Session,
        request: TurnInputRequest,
        metadata: AgentQueueTurnMetadata,
    ) -> CodexResult<ObservedTurnInputSubmission> {
        self.submit_observed_input(session, request, TurnInputMode::StartIfIdle, Some(metadata))
            .await
    }

    async fn submit_observed_input(
        &self,
        session: &Session,
        mut request: TurnInputRequest,
        mode: TurnInputMode,
        queue_metadata: Option<AgentQueueTurnMetadata>,
    ) -> CodexResult<ObservedTurnInputSubmission> {
        if !self
            .session
            .upgrade()
            .is_some_and(|target| std::ptr::eq(target.as_ref(), session))
        {
            return Err(CodexErr::InvalidRequest(
                "input admission belongs to another session".to_string(),
            ));
        }
        let submission_id = new_submission_id();
        let mut admission = session.register_input_turn_admission(submission_id.clone());
        let (queue_start_permit, input_persisted) = match queue_metadata {
            Some(metadata) => {
                let (permit, persisted) =
                    session.register_queued_input_start(&submission_id, metadata);
                (Some(permit), Some(persisted))
            }
            None => (None, None),
        };
        let (reply, routing) = oneshot::channel();
        let trace = request.trace.take();
        self.submit_with_id(Submission {
            id: submission_id.clone(),
            op: Op::TurnInput {
                request: Box::new(request),
                mode,
                reply,
            },
            trace,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await?;
        admission.mark_submitted();
        let routing = match routing.await {
            Ok(Ok(routing)) => routing,
            Ok(Err(error)) => {
                let warning = format!(
                    "input routing failed after enqueue: {error}; admission is unknown; do not resubmit; reload required"
                );
                session.quarantine_history(warning.clone());
                return Ok(ObservedTurnInputSubmission::Indeterminate {
                    submission_id,
                    warning,
                });
            }
            Err(_) => {
                let warning = "input routing receipt was lost after enqueue; do not resubmit; reload required".to_string();
                session.quarantine_history(warning.clone());
                return Ok(ObservedTurnInputSubmission::Indeterminate {
                    submission_id,
                    warning,
                });
            }
        };
        match routing {
            TurnInputSubmission::NotSubmitted { reason } => {
                session.reject_input_turn_admission(
                    &submission_id,
                    CodexErr::InvalidRequest("input was not submitted".to_string()),
                );
                Ok(ObservedTurnInputSubmission::NotSubmitted { reason })
            }
            TurnInputSubmission::Started { turn_id } | TurnInputSubmission::Steered { turn_id } => {
                let detail = match admission.recv().await {
                    Some(Ok(resolution)) if resolution.target_turn_id == turn_id => {
                        return Ok(ObservedTurnInputSubmission::Admitted {
                            submission_id,
                            resolution,
                            queue_start_permit,
                            input_persisted,
                        });
                    }
                    Some(Ok(_)) => "admission receipt belonged to another turn".to_string(),
                    Some(Err(error)) => error.to_string(),
                    None => "admission receipt was lost".to_string(),
                };
                let warning = format!(
                    "input accepted by turn {turn_id}, but response observation failed: {detail}; do not resubmit; reload required"
                );
                session.quarantine_history(warning.clone());
                Ok(ObservedTurnInputSubmission::AdmittedWithoutObservation {
                    submission_id,
                    target_turn_id: turn_id,
                    warning,
                })
            }
        }
    }
}
