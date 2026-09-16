use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::grpc;
use codex_code_mode_protocol::grpc::code_mode_host_client::CodeModeHostClient;
use http_body_util::Full;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::body::Body;
use tonic::codegen::Bytes;
use tonic::codegen::http;
use tonic::transport::Endpoint;
use tower::service_fn;
use tower::util::BoxCloneSyncService;

use super::super::SessionInner;
use super::super::state::SessionState;
use super::super::transport::SharedTransport;
use crate::remote_session::wait_for_watch;

#[derive(Default)]
struct HeldNotification {
    started: CancellationToken,
    release: CancellationToken,
    finished: CancellationToken,
}

impl CodeModeSessionDelegate for HeldNotification {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Err("unused invocation".to_string()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        cancellation: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            self.started.cancel();
            cancellation.cancelled().await;
            self.release.cancelled().await;
            self.finished.cancel();
            Ok(())
        })
    }
}

fn session(failed_closes: usize, attempts: Arc<AtomicUsize>) -> Arc<SessionInner> {
    let transport = service_fn(move |request: http::Request<Body>| {
        let attempts = Arc::clone(&attempts);
        async move {
            assert!(request.uri().path().ends_with("/CloseSession"));
            if attempts.fetch_add(1, Ordering::SeqCst) < failed_closes {
                return Ok::<_, io::Error>(tonic::Status::unavailable("retry close").into_http());
            }
            Ok(http::Response::builder()
                .header("content-type", "application/grpc")
                .header("grpc-status", "0")
                .body(Body::new(Full::new(Bytes::from_static(&[0, 0, 0, 0, 0]))))
                .expect("empty close response"))
        }
    });
    Arc::new(SessionInner {
        id: "session-one".to_string(),
        client: CodeModeHostClient::new(BoxCloneSyncService::new(transport)),
        runtime: tokio::runtime::Handle::current(),
        state: Mutex::new(SessionState::default()),
        wait_slots: Mutex::new(HashMap::new()),
        shutdown_requested: AtomicBool::new(false),
        shutdown_result: Mutex::new(None),
        durable_result: Mutex::new(None),
        close_confirmed: AtomicBool::new(false),
        callback_panicked: AtomicBool::new(false),
        durable_requested: AtomicBool::new(false),
        stopped: CancellationToken::new(),
        stream_tasks: TaskTracker::new(),
        callback_tasks: TaskTracker::new(),
        _transport: Arc::new(SharedTransport::with_channel(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        )),
    })
}

#[tokio::test]
async fn failed_close_is_retried_even_after_local_state_is_closed() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let inner = session(/*failed_closes*/ 1, Arc::clone(&attempts));
    let error = wait_for_watch(inner.request_durable_shutdown())
        .await
        .expect_err("first close fails");
    assert!(error.starts_with("gRPC code-mode durable session shutdown failed:"));
    assert!(error.contains("retry close"));
    assert!(!inner.close_confirmed.load(Ordering::Acquire));
    assert_eq!(
        wait_for_watch(inner.request_durable_shutdown()).await,
        Ok(())
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn durable_shutdown_waits_for_accepted_notification_after_remote_close_ack() {
    let delegate = Arc::new(HeldNotification::default());
    let attempts = Arc::new(AtomicUsize::new(0));
    let inner = session(/*failed_closes*/ 0, Arc::clone(&attempts));
    inner
        .state
        .lock()
        .expect("state")
        .begin_execution(
            &grpc::ExecuteRequest {
                session_id: inner.id.clone(),
                execution_id: "execution".to_string(),
                tool_call_id: "call".to_string(),
                ..Default::default()
            },
            delegate.clone(),
        )
        .expect("execution");
    inner
        .handle_notification(grpc::Notification {
            notification_id: uuid::Uuid::new_v4().to_string(),
            execution_id: "execution".to_string(),
            cell_id: "cell".to_string(),
            call_id: "call".to_string(),
            text: "accepted notification".to_string(),
        })
        .expect("notification admitted");
    delegate.started.cancelled().await;
    let first = inner.request_durable_shutdown();
    drop(first);
    inner.stopped.cancelled().await;
    assert!(inner.close_confirmed.load(Ordering::Acquire));
    assert!(!delegate.finished.is_cancelled());
    let result = inner.request_durable_shutdown();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), wait_for_watch(result))
            .await
            .is_err()
    );
    delegate.release.cancel();
    assert_eq!(
        wait_for_watch(inner.request_durable_shutdown()).await,
        Ok(())
    );
    assert!(delegate.finished.is_cancelled());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
