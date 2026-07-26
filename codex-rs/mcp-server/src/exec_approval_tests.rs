use pretty_assertions::assert_eq;
use serde_json::json;

use super::ApprovalResponse;
use super::classify_approval_response;
use crate::outgoing_message::ClientResponse;

#[test]
fn approval_response_received_before_deadline_is_on_time() {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let response = ClientResponse {
        received_at: deadline - std::time::Duration::from_millis(1),
        result: json!({"decision": "approved"}),
    };

    let ApprovalResponse::OnTime(Ok(result)) = classify_approval_response(Ok(response), deadline)
    else {
        panic!("response received before deadline should be accepted");
    };
    assert_eq!(result, json!({"decision": "approved"}));
}

#[test]
fn approval_response_received_at_deadline_is_timed_out() {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let response = ClientResponse {
        received_at: deadline,
        result: json!({"decision": "approved"}),
    };

    assert!(matches!(
        classify_approval_response(Ok(response), deadline),
        ApprovalResponse::TimedOut
    ));
}
