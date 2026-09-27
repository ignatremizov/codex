//! A request observed during predicate evaluation is not necessarily accepted by that mock.

use super::ev_completed;
use super::mount_responder_once_match;
use super::mount_response_once_match;
use super::mount_sse_once_match;
use super::mount_sse_once_match_with_delay;
use super::sse;
use super::sse_response;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use wiremock::MockServer;
use wiremock::matchers::body_partial_json;

#[derive(Clone, Copy, Debug)]
enum Helper {
    Response,
    Responder,
    Sse,
    DelayedSse,
}

#[tokio::test]
async fn matching_helpers_record_only_their_accepted_request() -> anyhow::Result<()> {
    for helper in [
        Helper::Response,
        Helper::Responder,
        Helper::Sse,
        Helper::DelayedSse,
    ] {
        let server = MockServer::start().await;
        let matcher = body_partial_json(json!({"marker": "accepted"}));
        let body = sse(vec![ev_completed("matching-response")]);
        let mock = match helper {
            Helper::Response => {
                mount_response_once_match(&server, matcher, sse_response(body)).await
            }
            Helper::Responder => {
                mount_responder_once_match(&server, matcher, move |_: &wiremock::Request| {
                    sse_response(body.clone())
                })
                .await
            }
            Helper::Sse => mount_sse_once_match(&server, matcher, body).await,
            Helper::DelayedSse => {
                mount_sse_once_match_with_delay(&server, matcher, body, Duration::from_millis(1))
                    .await
            }
        };
        let url = format!("{}/v1/responses", server.uri());
        let client = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
            .build_client(&url, ClientRouteClass::Other)?;
        let rejected = json!({"model": "test-model", "input": [], "marker": "unrelated"});
        let response = client.post(&url).json(&rejected).send().await?;
        assert_eq!(response.status().as_u16(), 404);
        let _ = response.bytes().await?;
        assert!(
            mock.requests().is_empty(),
            "{helper:?} recorded a rejected request"
        );

        let accepted = json!({
            "model": "test-model",
            "marker": "accepted",
            "input": [
                {"type": "custom_tool_call", "name": "exec", "call_id": "cell", "input": "text(1)"},
                {"type": "custom_tool_call_output", "call_id": "cell", "output": "1"}
            ]
        });
        let response = client.post(&url).json(&accepted).send().await?;
        assert_eq!(response.status().as_u16(), 200);
        let _ = response.bytes().await?;
        assert_eq!(mock.single_request().body_json(), accepted);

        // A consumed one-shot mock must not admit or record a second matching request.
        let response = client.post(&url).json(&accepted).send().await?;
        assert_eq!(response.status().as_u16(), 404);
        let _ = response.bytes().await?;
        assert_eq!(mock.single_request().body_json(), accepted);
    }
    Ok(())
}
