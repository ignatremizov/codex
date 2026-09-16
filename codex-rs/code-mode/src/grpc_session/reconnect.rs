use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSession;
use codex_code_mode_protocol::CodeModeSessionCellExecutionLimits;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeSessionResultFuture;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::GrpcCodeModeSession;
use super::GrpcCodeModeSessionProvider;
use super::generation;
use super::generation::GenerationDelegate;
use crate::remote_session::ShutdownResultReceiver;
use crate::remote_session::wait_for_watch;

mod opening;

pub(super) struct ReconnectableSession {
    inner: Arc<ReconnectInner>,
}

struct ReconnectInner {
    provider: GrpcCodeModeSessionProvider,
    limits: CodeModeSessionCellExecutionLimits,
    binding: Mutex<Option<SessionBinding>>,
    retired: Mutex<Vec<SessionBinding>>,
    opening_permit: Arc<Semaphore>,
    opening: Mutex<Option<opening::OpeningAttempt>>,
    next_generation: AtomicU64,
    shutdown_requested: CancellationToken,
    shutdown_result: Mutex<Option<ShutdownResultReceiver>>,
    durable_result: Mutex<Option<ShutdownResultReceiver>>,
}

#[derive(Clone)]
struct SessionBinding {
    session: Arc<GrpcCodeModeSession>,
    generation: u64,
}

impl ReconnectableSession {
    pub(super) fn new(
        provider: GrpcCodeModeSessionProvider,
        limits: CodeModeSessionCellExecutionLimits,
    ) -> Self {
        Self {
            inner: Arc::new(ReconnectInner {
                provider,
                limits,
                binding: Mutex::new(None),
                retired: Mutex::new(Vec::new()),
                opening_permit: Arc::new(Semaphore::new(/*permits*/ 1)),
                opening: Mutex::new(None),
                next_generation: AtomicU64::new(1),
                shutdown_requested: CancellationToken::new(),
                shutdown_result: Mutex::new(None),
                durable_result: Mutex::new(None),
            }),
        }
    }

    pub(super) async fn initialize(&self) -> Result<(), String> {
        self.inner.get_or_open_binding().await.map(|_| ())
    }
}

impl CodeModeSession for ReconnectableSession {
    fn prewarm<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(self.initialize())
    }

    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            let binding = self.inner.get_or_open_binding().await?;
            let delegate = Arc::new(GenerationDelegate {
                delegate,
                generation: binding.generation,
            });
            let started = binding.session.execute(request, delegate).await?;
            Ok(generation::public_started_cell(binding.generation, started))
        })
    }

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            let binding = self.inner.get_or_open_binding().await?;
            let request = WaitRequest {
                cell_id: generation::remote_cell_id(binding.generation, &request.cell_id)?,
                yield_time_ms: request.yield_time_ms,
            };
            let outcome = binding.session.wait(request).await?;
            Ok(generation::public_wait_outcome(binding.generation, outcome))
        })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            let binding = self.inner.get_or_open_binding().await?;
            let cell_id = generation::remote_cell_id(binding.generation, &cell_id)?;
            let outcome = binding.session.terminate(cell_id).await?;
            Ok(generation::public_wait_outcome(binding.generation, outcome))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(wait_for_watch(self.inner.request_shutdown()))
    }

    fn shutdown_durably<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(wait_for_watch(self.inner.request_durable_shutdown()))
    }
}

impl Drop for ReconnectableSession {
    fn drop(&mut self) {
        if tokio::runtime::Handle::try_current().is_ok() {
            self.inner.request_shutdown();
        }
    }
}

impl ReconnectInner {
    fn live_binding(&self) -> Option<SessionBinding> {
        self.binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|binding| !binding.session.inner.stopped.is_cancelled())
            .cloned()
    }

    fn request_shutdown(self: &Arc<Self>) -> ShutdownResultReceiver {
        let established_shutdown = {
            let _opening = self.opening.lock().unwrap_or_else(PoisonError::into_inner);
            let binding = self.binding.lock().unwrap_or_else(PoisonError::into_inner);
            self.shutdown_requested.cancel();
            binding
                .as_ref()
                .map(|binding| binding.session.inner.request_shutdown())
        };
        let mut result = self
            .shutdown_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(receiver) = result.as_ref() {
            return receiver.clone();
        }

        // Fast shutdown still waits for an established binding's fast close, but
        // an opening without an ID must not delay interruption. The owned task
        // below retains cleanup; only durable shutdown proves it has completed.
        let receiver = established_shutdown.unwrap_or_else(|| watch::channel(Some(Ok(()))).1);
        *result = Some(receiver.clone());
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let opening_permit = match inner.opening_permit.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!("gRPC code-mode session opening coordinator closed");
                    return;
                }
            };
            let binding = inner
                .binding
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            drop(opening_permit);
            if let Some(binding) = binding
                && let Err(error) = wait_for_watch(binding.session.inner.request_shutdown()).await
            {
                tracing::warn!(%error, "gRPC code-mode background fast close failed");
            }
        });
        receiver
    }

    fn request_durable_shutdown(self: &Arc<Self>) -> ShutdownResultReceiver {
        let opening = {
            let opening = self.opening.lock().unwrap_or_else(PoisonError::into_inner);
            self.shutdown_requested.cancel();
            opening.clone()
        };
        let mut attempt = self
            .durable_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(receiver) = attempt.as_ref()
            && !matches!(&*receiver.borrow(), Some(Err(_)))
        {
            return receiver.clone();
        }
        let (sender, receiver) = watch::channel(None);
        *attempt = Some(receiver.clone());
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let result = async {
                let _permit =
                    inner.opening_permit.acquire().await.map_err(|_| {
                        "gRPC code-mode session opening coordinator closed".to_string()
                    })?;
                let mut bindings = inner
                    .retired
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                if let Some(binding) = inner
                    .binding
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                {
                    bindings.push(binding);
                }
                if let Some(opening) = opening
                    && let Err(error) = wait_for_watch(opening.result).await
                    && opening.unidentified_backend.load(Ordering::Acquire)
                {
                    return Err(error);
                }
                let results = futures::future::join_all(
                    bindings
                        .iter()
                        .map(|binding| binding.session.shutdown_durably()),
                )
                .await;
                results
                    .into_iter()
                    .collect::<Result<Vec<()>, String>>()
                    .map(|_| ())
            }
            .await;
            sender.send_replace(Some(result));
        });
        receiver
    }
}
