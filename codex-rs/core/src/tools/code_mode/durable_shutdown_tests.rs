use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use crate::config::CodeModeConfig;
use crate::tools::code_mode::CodeModeService;

#[derive(Default)]
struct Provider {
    session: Arc<GatedSession>,
    opens: AtomicUsize,
    opening: CancellationToken,
    allow_open: CancellationToken,
    fail_create: AtomicBool,
}

impl CodeModeSessionProvider for Provider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.opening.cancel();
            self.allow_open.cancelled().await;
            if self.fail_create.load(Ordering::SeqCst) {
                return Err("partial initialization failed".to_string());
            }
            Ok(self.session.clone() as Arc<dyn CodeModeSession>)
        })
    }
}

#[derive(Default)]
struct GatedSession {
    attempts: AtomicUsize,
    failures: AtomicUsize,
    closing: CancellationToken,
    allow_close: CancellationToken,
}

impl CodeModeSession for GatedSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async { Err("unused execution".to_string()) })
    }

    fn wait<'a>(&'a self, _request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("unused wait".to_string()) })
    }

    fn terminate<'a>(&'a self, _cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("unused termination".to_string()) })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn shutdown_durably<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            self.closing.cancel();
            self.allow_close.cancelled().await;
            if attempt < self.failures.load(Ordering::SeqCst) {
                Err("retry close".to_string())
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn unused_service_does_not_open_a_provider_during_durable_shutdown() {
    let provider = Arc::new(Provider::default());
    let service = CodeModeService::new(
        provider.clone(),
        &CodeModeConfig::default(),
        /*executed_tool_calls*/ None,
    );
    assert_eq!(service.shutdown_durably().await, Ok(()));
    assert_eq!(provider.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_factory_without_cleanup_handle_is_not_treated_as_unused() {
    let provider = Arc::new(Provider::default());
    provider.allow_open.cancel();
    provider.fail_create.store(true, Ordering::SeqCst);
    let service = CodeModeService::new(
        provider.clone(),
        &CodeModeConfig::default(),
        /*executed_tool_calls*/ None,
    );
    assert_eq!(
        service.session().await.err(),
        Some("partial initialization failed".to_string())
    );
    provider.fail_create.store(false, Ordering::SeqCst);
    assert_eq!(
        service.session().await.err(),
        Some("partial initialization failed".to_string())
    );
    assert_eq!(provider.opens.load(Ordering::SeqCst), 1);
    assert_eq!(
        service.shutdown_durably().await,
        Err("partial initialization failed".to_string())
    );
    assert_eq!(
        service.shutdown_durably().await,
        Err("partial initialization failed".to_string())
    );
}

#[tokio::test]
async fn failed_durable_close_retries_the_same_provider_session() {
    let provider = Arc::new(Provider::default());
    provider.allow_open.cancel();
    provider.session.allow_close.cancel();
    provider.session.failures.store(1, Ordering::SeqCst);
    let service = CodeModeService::new(
        provider.clone(),
        &CodeModeConfig::default(),
        /*executed_tool_calls*/ None,
    );
    service.session().await.expect("initialize");

    assert_eq!(
        service.shutdown_durably().await,
        Err("retry close".to_string())
    );
    assert_eq!(service.shutdown_durably().await, Ok(()));
    assert_eq!(
        (
            provider.opens.load(Ordering::SeqCst),
            provider.session.attempts.load(Ordering::SeqCst),
        ),
        (1, 2),
    );
}

#[tokio::test]
async fn dropped_shutdown_caller_does_not_drop_or_duplicate_the_attempt() {
    let provider = Arc::new(Provider::default());
    provider.allow_open.cancel();
    let service = Arc::new(CodeModeService::new(
        provider.clone(),
        &CodeModeConfig::default(),
        None,
    ));
    service.session().await.expect("initialize");
    let first_service = Arc::clone(&service);
    let first = tokio::spawn(async move { first_service.shutdown_durably().await });
    provider.session.closing.cancelled().await;
    first.abort();
    let _ = first.await;

    let second_service = Arc::clone(&service);
    let mut second = tokio::spawn(async move { second_service.shutdown_durably().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    provider.session.allow_close.cancel();
    assert_eq!(second.await.expect("shutdown waiter"), Ok(()));
    assert_eq!(provider.session.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn abandoned_initialization_is_retained_and_closed_before_durable_success() {
    let provider = Arc::new(Provider::default());
    let service = Arc::new(CodeModeService::new(
        provider.clone(),
        &CodeModeConfig::default(),
        None,
    ));
    let opening_service = Arc::clone(&service);
    let opening = tokio::spawn(async move { opening_service.session().await.map(|_| ()) });
    provider.opening.cancelled().await;
    opening.abort();
    let _ = opening.await;

    let closing_service = Arc::clone(&service);
    let mut closing = tokio::spawn(async move { closing_service.shutdown_durably().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut closing)
            .await
            .is_err()
    );
    provider.allow_open.cancel();
    provider.session.closing.cancelled().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut closing)
            .await
            .is_err()
    );
    provider.session.allow_close.cancel();
    assert_eq!(closing.await.expect("shutdown waiter"), Ok(()));
    assert_eq!(provider.opens.load(Ordering::SeqCst), 1);
}
