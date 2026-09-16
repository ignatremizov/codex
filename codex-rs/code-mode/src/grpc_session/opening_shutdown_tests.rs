use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeSessionProvider;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::grpc;
use codex_code_mode_protocol::grpc::code_mode_host_client::CodeModeHostClient;
use http_body_util::BodyExt;
use http_body_util::Full;
use pretty_assertions::assert_eq;
use prost::Message;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tonic::body::Body;
use tonic::codegen::Bytes;
use tonic::codegen::http;
use tonic::transport::Endpoint;
use tower::service_fn;
use tower::util::BoxCloneSyncService;

use super::super::GrpcCodeModeSessionProvider;
use super::SharedTransport;
use super::TransportEndpoint;

#[derive(Default)]
struct Host {
    opening: CancellationToken,
    release_open: CancellationToken,
    opens: AtomicUsize,
    closes: AtomicUsize,
    fail_open: AtomicBool,
}

fn provider(host: Arc<Host>) -> GrpcCodeModeSessionProvider {
    let transport = service_fn(move |request: http::Request<Body>| {
        let host = Arc::clone(&host);
        async move {
            let is_open = request.uri().path().ends_with("/OpenSession");
            let bytes = match request.uri().path().rsplit('/').next().expect("method") {
                "OpenSession" => {
                    host.opens.fetch_add(1, Ordering::SeqCst);
                    host.opening.cancel();
                    host.release_open.cancelled().await;
                    if host.fail_open.load(Ordering::SeqCst) {
                        return Ok::<_, io::Error>(
                            tonic::Status::unavailable("opening result was lost").into_http(),
                        );
                    }
                    grpc::SessionEvent {
                        event: Some(grpc::session_event::Event::Opened(grpc::SessionOpened {
                            session_id: "retained-session".to_string(),
                        })),
                    }
                    .encode_to_vec()
                }
                "SubscribeToToolCalls" => {
                    return Ok::<_, io::Error>(
                        tonic::Status::unavailable("subscription failed").into_http(),
                    );
                }
                "CloseSession" => {
                    let body = request
                        .into_body()
                        .collect()
                        .await
                        .expect("request body")
                        .to_bytes();
                    let close =
                        grpc::CloseSessionRequest::decode(&body[5..]).expect("close request");
                    assert_eq!(close.session_id, "retained-session");
                    if host.closes.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Ok(tonic::Status::unavailable("close failed").into_http());
                    }
                    grpc::CloseSessionResponse {}.encode_to_vec()
                }
                method => panic!("unexpected backend operation {method}"),
            };
            let mut framed = vec![0];
            framed.extend_from_slice(
                &u32::try_from(bytes.len())
                    .expect("message length")
                    .to_be_bytes(),
            );
            framed.extend(bytes);
            let body = Full::new(Bytes::from(framed));
            let body = if is_open {
                Body::new(body.with_trailers(std::future::pending::<
                    Option<Result<http::HeaderMap, std::convert::Infallible>>,
                >()))
            } else {
                Body::new(body)
            };
            Ok(http::Response::builder()
                .header("content-type", "application/grpc")
                .header("grpc-status", "0")
                .body(body)
                .expect("grpc response"))
        }
    });
    GrpcCodeModeSessionProvider::from_transport(SharedTransport {
        endpoint: TransportEndpoint::Connected(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        ),
        client: OnceCell::new_with(Some(CodeModeHostClient::new(BoxCloneSyncService::new(
            transport,
        )))),
    })
}

fn request() -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call".to_string(),
        enabled_tools: Vec::new(),
        source: "text('test')".to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn subscription_failure_retains_created_session_when_fast_close_fails() {
    let host = Arc::new(Host::default());
    host.release_open.cancel();
    let provider = provider(Arc::clone(&host));
    let session = provider
        .create_owned_session(Arc::new(crate::NoopCodeModeSessionDelegate))
        .await
        .expect("logical session");
    assert!(session.execute(request()).await.is_err());
    // A reconnect attempt can fail while retiring the known previous binding.
    // That failure must not prevent durable shutdown from retrying its close.
    assert!(session.execute(request()).await.is_err());
    assert_eq!(session.shutdown_durably().await, Ok(()));
    assert_eq!(
        (
            host.opens.load(Ordering::SeqCst),
            host.closes.load(Ordering::SeqCst)
        ),
        (1, 2),
    );
}

#[tokio::test]
async fn dropped_execute_caller_during_open_rpc_does_not_abandon_remote_cleanup() {
    let host = Arc::new(Host::default());
    let provider = provider(Arc::clone(&host));
    let session = provider
        .create_owned_session(Arc::new(crate::NoopCodeModeSessionDelegate))
        .await
        .expect("logical session");
    let executing = Arc::clone(&session);
    let caller = tokio::spawn(async move { executing.execute(request()).await.map(|_| ()) });
    host.opening.cancelled().await;
    caller.abort();
    let _ = caller.await;
    let closing = Arc::clone(&session);
    let mut shutdown = tokio::spawn(async move { closing.shutdown_durably().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    host.release_open.cancel();
    assert_eq!(shutdown.await.expect("durable shutdown"), Ok(()));
    assert_eq!(
        (
            host.opens.load(Ordering::SeqCst),
            host.closes.load(Ordering::SeqCst)
        ),
        (1, 2),
    );
}

#[tokio::test]
async fn unidentified_failed_open_cannot_be_overwritten_by_a_later_successful_generation() {
    let host = Arc::new(Host::default());
    host.release_open.cancel();
    host.fail_open.store(true, Ordering::SeqCst);
    let provider = provider(Arc::clone(&host));
    let session = provider
        .create_owned_session(Arc::new(crate::NoopCodeModeSessionDelegate))
        .await
        .expect("logical session");
    let first_error = session
        .execute(request())
        .await
        .err()
        .expect("uncertain opening");
    host.fail_open.store(false, Ordering::SeqCst);
    assert_eq!(
        session.execute(request()).await.err(),
        Some(first_error.clone())
    );
    assert_eq!(session.shutdown_durably().await, Err(first_error));
    assert_eq!(
        (
            host.opens.load(Ordering::SeqCst),
            host.closes.load(Ordering::SeqCst)
        ),
        (1, 0),
    );
}
