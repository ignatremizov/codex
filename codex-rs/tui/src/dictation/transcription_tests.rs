use super::*;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const OK: u16 = 200;
const UNAUTHORIZED: u16 = 401;

enum Refresh {
    Http(String),
    Fail,
    SwitchAccount,
    Pending(Arc<Notify>),
}

struct SyntheticAuth {
    refresh: Refresh,
    refreshes: AtomicUsize,
    credentials: Mutex<UploadCredentials>,
}

impl SyntheticAuth {
    fn new(refresh: Refresh) -> Self {
        Self {
            refresh,
            refreshes: AtomicUsize::new(/*v*/ 0),
            credentials: Mutex::new(UploadCredentials {
                bearer: "old-test-token".into(),
                account_id: "test-account".into(),
                fedramp: true,
            }),
        }
    }
}

impl TranscriptionAuth for SyntheticAuth {
    async fn credentials(&self) -> Result<UploadCredentials, String> {
        let credentials = self.credentials.lock().expect("synthetic credentials");
        Ok(UploadCredentials {
            bearer: credentials.bearer.clone(),
            account_id: credentials.account_id.clone(),
            fedramp: credentials.fedramp,
        })
    }

    async fn refresh(&self) -> Result<(), String> {
        self.refreshes.fetch_add(/*val*/ 1, Ordering::SeqCst);
        match &self.refresh {
            // This is a synthetic authority, not the production OAuth endpoint/refresh adapter.
            Refresh::Http(endpoint) => {
                let response = pool()
                    .post(endpoint)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(response.text().await.map_err(|e| e.to_string())?);
                }
                let bearer = response.text().await.map_err(|e| e.to_string())?;
                self.credentials
                    .lock()
                    .expect("synthetic credentials")
                    .bearer = bearer;
                Ok(())
            }
            Refresh::Fail => Err("synthetic refresh rejected".into()),
            Refresh::SwitchAccount => {
                self.credentials
                    .lock()
                    .expect("synthetic credentials")
                    .account_id = "other-account".into();
                Ok(())
            }
            Refresh::Pending(entered) => {
                entered.notify_one();
                std::future::pending().await
            }
        }
    }
}

fn pool() -> RouteAwareClientPool {
    RouteAwareClientPool::new(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Api,
    )
}

fn audio() -> RecordedAudio {
    RecordedAudio {
        data: vec![100, 300, 200, 400],
        sample_rate: 48_000,
        channels: 2,
    }
}

#[test]
fn browser_adapter_accepts_only_browser_auth() {
    assert!(browser_credentials(None).is_err());
    assert!(browser_credentials(Some(CodexAuth::from_api_key("synthetic"))).is_err());
    let external = CodexAuth::from_external_chatgpt_tokens(
        "e30.eyJzdWIiOiJzeW50aGV0aWMifQ.signature",
        "test-account",
        /*chatgpt_plan_type*/ None,
    )
    .expect("synthetic external tokens");
    assert!(browser_credentials(Some(external)).is_err());
    let credentials = browser_credentials(Some(CodexAuth::create_dummy_chatgpt_auth_for_testing()))
        .expect("browser credentials");
    assert_eq!(credentials.account_id, "account_id");
}

#[tokio::test]
async fn multipart_upload_normalizes_wav_and_preserves_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/transcribe"))
        .and(header("authorization", "Bearer old-test-token"))
        .and(header("chatgpt-account-id", "test-account"))
        .and(header("oai-product-sku", "CODEX"))
        .and(header("originator", "Codex Desktop"))
        .and(header("x-openai-fedramp", "true"))
        .respond_with(
            ResponseTemplate::new(OK).set_body_json(serde_json::json!({"text": " hello "})),
        )
        .mount(&server)
        .await;
    let auth = SyntheticAuth::new(Refresh::Fail);
    let result = upload_with_deadline(
        &auth,
        &pool(),
        &format!("{}/transcribe", server.uri()),
        "test-account",
        audio(),
        CancellationToken::new(),
        OPERATION_TIMEOUT,
    )
    .await
    .expect("transcript");
    assert_eq!(result, "hello");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let content_type = request.headers["content-type"]
        .to_str()
        .expect("multipart header");
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .expect("boundary");
    let prefix = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"codex.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    );
    let mut expected = prefix.into_bytes();
    expected.extend_from_slice(&[
        82, 73, 70, 70, 38, 0, 0, 0, 87, 65, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1, 0,
        192, 93, 0, 0, 128, 187, 0, 0, 2, 0, 16, 0, 100, 97, 116, 97, 2, 0, 0, 0, 200, 0,
    ]);
    expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    assert_eq!(request.body, expected);
    assert!(
        request.headers["user-agent"]
            .to_str()
            .expect("user agent")
            .starts_with("Codex Desktop/26.609.41114 ")
    );
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unauthorized_refreshes_through_synthetic_http_authority_once() {
    for retry_status in [200, 401] {
        let server = MockServer::start().await;
        Mock::given(path("/transcribe"))
            .and(header("authorization", "Bearer old-test-token"))
            .respond_with(ResponseTemplate::new(UNAUTHORIZED).set_body_string("first unauthorized"))
            .mount(&server)
            .await;
        Mock::given(path("/refresh"))
            .respond_with(ResponseTemplate::new(OK).set_body_string("new-test-token"))
            .mount(&server)
            .await;
        Mock::given(path("/transcribe"))
            .and(header("authorization", "Bearer new-test-token"))
            .and(header("chatgpt-account-id", "test-account"))
            .and(header("x-openai-fedramp", "true"))
            .respond_with(ResponseTemplate::new(retry_status).set_body_string(
                if retry_status == 200 {
                    r#"{"text":"retried"}"#
                } else {
                    "second unauthorized"
                },
            ))
            .mount(&server)
            .await;
        let auth = SyntheticAuth::new(Refresh::Http(format!("{}/refresh", server.uri())));
        let result = upload_with_deadline(
            &auth,
            &pool(),
            &format!("{}/transcribe", server.uri()),
            "test-account",
            audio(),
            CancellationToken::new(),
            OPERATION_TIMEOUT,
        )
        .await;
        if retry_status == 200 {
            assert_eq!(result.expect("retry"), "retried");
        } else {
            assert!(
                matches!(result, Err(TranscriptionFailure::Failed(ref error))
                if error.contains("second unauthorized"))
            );
        }
        assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.expect("requests").len(), 3);
    }
}

#[tokio::test]
async fn failures_preserve_complete_body_and_refresh_diagnostic() {
    for status in [401, 503] {
        let server = MockServer::start().await;
        let body = "diagnostic".repeat(4096);
        Mock::given(path("/transcribe"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.clone()))
            .mount(&server)
            .await;
        let auth = SyntheticAuth::new(Refresh::Fail);
        let result = upload_with_deadline(
            &auth,
            &pool(),
            &format!("{}/transcribe", server.uri()),
            "test-account",
            audio(),
            CancellationToken::new(),
            OPERATION_TIMEOUT,
        )
        .await;
        let Err(TranscriptionFailure::Failed(error)) = result else {
            panic!("expected failure");
        };
        assert!(error.contains(&body));
        assert_eq!(error.contains("synthetic refresh rejected"), status == 401);
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }
}

#[tokio::test]
async fn account_switch_prevents_retry() {
    let server = MockServer::start().await;
    Mock::given(path("/transcribe"))
        .respond_with(ResponseTemplate::new(UNAUTHORIZED).set_body_string("original diagnostic"))
        .mount(&server)
        .await;
    let auth = SyntheticAuth::new(Refresh::SwitchAccount);
    let result = upload_with_deadline(
        &auth,
        &pool(),
        &format!("{}/transcribe", server.uri()),
        "test-account",
        audio(),
        CancellationToken::new(),
        OPERATION_TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, Err(TranscriptionFailure::Failed(ref error))
        if error.contains("account changed") && error.contains("original diagnostic"))
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn cancellation_during_refresh_prevents_retry() {
    let server = MockServer::start().await;
    Mock::given(path("/transcribe"))
        .respond_with(ResponseTemplate::new(UNAUTHORIZED))
        .mount(&server)
        .await;
    let entered = Arc::new(Notify::new());
    let auth = SyntheticAuth::new(Refresh::Pending(Arc::clone(&entered)));
    let cancellation = CancellationToken::new();
    let http = pool();
    let endpoint = format!("{}/transcribe", server.uri());
    let future = upload_with_deadline(
        &auth,
        &http,
        &endpoint,
        "test-account",
        audio(),
        cancellation.clone(),
        OPERATION_TIMEOUT,
    );
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("upload ended before refresh: {result:?}"),
        _ = entered.notified() => cancellation.cancel(),
    }
    assert!(matches!(future.await, Err(TranscriptionFailure::Canceled)));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn cancellation_and_deadline_cover_pending_http() {
    let server = MockServer::start().await;
    Mock::given(path("/transcribe"))
        .respond_with(ResponseTemplate::new(OK).set_delay(Duration::from_secs(/*secs*/ 30)))
        .mount(&server)
        .await;
    let auth = SyntheticAuth::new(Refresh::Fail);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let endpoint = format!("{}/transcribe", server.uri());
    let result = upload_with_deadline(
        &auth,
        &pool(),
        &endpoint,
        "test-account",
        audio(),
        cancellation,
        OPERATION_TIMEOUT,
    )
    .await;
    assert!(matches!(result, Err(TranscriptionFailure::Canceled)));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    let result = upload_with_deadline(
        &auth,
        &pool(),
        &endpoint,
        "test-account",
        audio(),
        CancellationToken::new(),
        Duration::from_millis(/*millis*/ 50),
    )
    .await;
    assert!(matches!(result, Err(TranscriptionFailure::TimedOut)));
}

#[tokio::test]
async fn malformed_and_empty_successes_are_errors() {
    for body in ["not json", r#"{"text":"   "}"#] {
        let server = MockServer::start().await;
        Mock::given(path("/transcribe"))
            .respond_with(ResponseTemplate::new(OK).set_body_string(body))
            .mount(&server)
            .await;
        let auth = SyntheticAuth::new(Refresh::Fail);
        assert!(matches!(
            upload_with_deadline(
                &auth,
                &pool(),
                &format!("{}/transcribe", server.uri()),
                "test-account",
                audio(),
                CancellationToken::new(),
                OPERATION_TIMEOUT,
            )
            .await,
            Err(TranscriptionFailure::Failed(_))
        ));
    }
}

#[tokio::test]
async fn cancellation_covers_credential_acquisition() {
    struct PendingAuth(Notify);
    impl TranscriptionAuth for PendingAuth {
        async fn credentials(&self) -> Result<UploadCredentials, String> {
            self.0.notify_one();
            std::future::pending().await
        }
        async fn refresh(&self) -> Result<(), String> {
            Err("refresh must not be reached".into())
        }
    }
    let auth = PendingAuth(Notify::new());
    let cancellation = CancellationToken::new();
    let http = pool();
    let future = upload_with_deadline(
        &auth,
        &http,
        "http://127.0.0.1:1/transcribe",
        "test-account",
        audio(),
        cancellation.clone(),
        OPERATION_TIMEOUT,
    );
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("credentials unexpectedly completed: {result:?}"),
        _ = auth.0.notified() => cancellation.cancel(),
    }
    assert!(matches!(future.await, Err(TranscriptionFailure::Canceled)));
}

#[tokio::test]
async fn cancellation_covers_partial_response_body() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let endpoint = format!(
        "http://{}/transcribe",
        listener.local_addr().expect("address")
    );
    let (body_started, body_received) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let mut bytes = vec![0; 4096];
        let mut request = Vec::new();
        loop {
            let count = socket.read(&mut bytes).await.expect("request bytes");
            assert_ne!(count, 0);
            request.extend_from_slice(&bytes[..count]);
            if request
                .windows(/*size*/ 4)
                .any(|window| window == b"\r\n\r\n")
            {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{")
            .await
            .expect("partial body");
        let _ = body_started.send(());
        let _ = released.await;
    });
    let auth = SyntheticAuth::new(Refresh::Fail);
    let http = pool();
    let cancellation = CancellationToken::new();
    let future = upload_with_deadline(
        &auth,
        &http,
        &endpoint,
        "test-account",
        audio(),
        cancellation.clone(),
        OPERATION_TIMEOUT,
    );
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("partial body unexpectedly completed: {result:?}"),
        result = body_received => {
            result.expect("body started");
            cancellation.cancel();
        }
    }
    assert!(matches!(future.await, Err(TranscriptionFailure::Canceled)));
    let _ = release.send(());
    server.await.expect("server");
}

#[tokio::test]
async fn changed_account_before_upload_sends_nothing() {
    let server = MockServer::start().await;
    let auth = SyntheticAuth::new(Refresh::Fail);
    let result = upload_with_deadline(
        &auth,
        &pool(),
        &format!("{}/transcribe", server.uri()),
        "different-recording-account",
        audio(),
        CancellationToken::new(),
        OPERATION_TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, Err(TranscriptionFailure::Failed(ref error)) if error.contains("account changed"))
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[test]
fn malformed_audio_is_rejected_and_final_short_audio_is_retained() {
    for audio in [
        RecordedAudio {
            data: Vec::new(),
            sample_rate: UPLOAD_RATE,
            channels: 1,
        },
        RecordedAudio {
            data: vec![1],
            sample_rate: 0,
            channels: 1,
        },
        RecordedAudio {
            data: vec![1],
            sample_rate: UPLOAD_RATE,
            channels: 0,
        },
        RecordedAudio {
            data: vec![1],
            sample_rate: UPLOAD_RATE,
            channels: 2,
        },
    ] {
        assert!(encode_audio(audio).is_err());
    }
    let wav = encode_audio(RecordedAudio {
        data: vec![-1],
        sample_rate: UPLOAD_RATE,
        channels: 1,
    })
    .expect("final sample");
    assert_eq!(wav.len(), 46);
    assert_eq!(&wav[44..], &(-1i16).to_le_bytes());
}
