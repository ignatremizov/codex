use super::is_model_capacity_error;
use codex_client::TransportError;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn only_structured_http_capacity_is_deferred_to_the_turn() {
    for (status, body, expected) in [
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"code": "server_is_overloaded"}}).to_string(),
            true,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"code": "slow_down"}}).to_string(),
            false,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "Selected model is at capacity."}}).to_string(),
            false,
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": {"code": "server_is_overloaded"}}).to_string(),
            false,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "not json".to_string(),
            false,
        ),
    ] {
        assert_eq!(
            is_model_capacity_error(&TransportError::Http {
                status,
                body: Some(body),
                url: None,
                headers: None,
                retry_after: None,
            }),
            expected,
        );
    }
}
