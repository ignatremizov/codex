use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn lost_durable_receipt_preserves_positive_admission_and_actual_turn() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    drop(sender);
    let mut submission = ResponseObservationSubmission {
        submission_id: "submission".into(),
        target_turn_id: Some("actual-turn".into()),
        input_outcome: UserAgentInputOutcome::Admitted,
        response_observation: ResponseObservationPolicy::default(),
        post_admission_warning: Some("source observation unavailable".into()),
    };
    submission.await_input_persistence(Some(receiver)).await;
    assert_eq!(
        (submission.input_outcome, submission.target_turn_id),
        (UserAgentInputOutcome::Admitted, Some("actual-turn".into()))
    );
    let warning = submission
        .post_admission_warning
        .expect("receipt failure warning");
    assert!(warning.contains("source observation unavailable"));
    assert!(warning.contains("do not resend"));
}

#[test]
fn unknown_input_is_not_converted_to_strict_success() {
    let submission = ResponseObservationSubmission {
        submission_id: "submission".into(),
        target_turn_id: None,
        input_outcome: UserAgentInputOutcome::Unknown,
        response_observation: ResponseObservationPolicy::default(),
        post_admission_warning: Some("input outcome unknown; do not resend".into()),
    };
    assert!(submission.into_strict_result().is_err());
}
