use pretty_assertions::assert_eq;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;

use super::*;

#[tokio::test]
async fn sqlite_sink_drops_low_level_opentelemetry_sdk_logs() {
    let codex_home =
        std::env::temp_dir().join(format!("codex-state-log-db-filter-{}", Uuid::new_v4()));
    let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let layer = start(runtime.clone());

    let guard = tracing_subscriber::registry()
        .with(
            layer
                .clone()
                .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
        )
        .set_default();

    tracing::trace!(target: "opentelemetry_sdk", "dropped-trace");
    tracing::debug!(target: "opentelemetry_sdk", "dropped-debug");
    tracing::info!(target: "opentelemetry_sdk", "retained-info");
    tracing::trace!(target: "codex_state", "retained-trace");

    layer.flush().await;
    drop(guard);

    let logs = runtime
        .query_logs(&crate::LogQuery::default())
        .await
        .expect("query logs after flush");
    assert_eq!(
        logs.iter()
            .map(|row| (
                row.level.as_str(),
                row.target.as_str(),
                row.message.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("INFO", "opentelemetry_sdk", Some("retained-info")),
            ("TRACE", "codex_state", Some("retained-trace")),
        ]
    );

    let _ = tokio::fs::remove_dir_all(codex_home).await;
}

#[tokio::test]
async fn sqlite_default_filter_drops_raw_response_payload_logs() {
    let codex_home =
        std::env::temp_dir().join(format!("codex-state-log-db-filter-{}", Uuid::new_v4()));
    let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let layer = start(runtime.clone());

    let guard = tracing_subscriber::registry()
        .with(layer.clone().with_filter(default_filter()))
        .set_default();

    tracing::trace!(
        target: "codex_api::raw_response_event",
        "dropped-raw-response-payload"
    );
    tracing::trace!(target: "codex_api::sse::responses", "retained-sse-trace");
    tracing::debug!(target: "codex_api::sse::responses", "retained-sse-debug");
    tracing::debug!(
        target: "codex_api::endpoint::responses_websocket",
        "retained-websocket-debug"
    );
    tracing::trace!(target: "codex_state", "retained-default-trace");

    layer.flush().await;
    drop(guard);

    let logs = runtime
        .query_logs(&crate::LogQuery::default())
        .await
        .expect("query logs after flush");
    assert_eq!(
        logs.iter()
            .map(|row| (
                row.level.as_str(),
                row.target.as_str(),
                row.message.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "TRACE",
                "codex_api::sse::responses",
                Some("retained-sse-trace")
            ),
            (
                "DEBUG",
                "codex_api::sse::responses",
                Some("retained-sse-debug")
            ),
            (
                "DEBUG",
                "codex_api::endpoint::responses_websocket",
                Some("retained-websocket-debug")
            ),
            ("TRACE", "codex_state", Some("retained-default-trace")),
        ]
    );

    let _ = tokio::fs::remove_dir_all(codex_home).await;
}
