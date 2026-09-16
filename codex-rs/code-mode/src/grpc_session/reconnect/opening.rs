//! One accepted opening attempt owns its RPCs independently of operation waiters.

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tokio::sync::watch;

use super::super::SHUTDOWN_ERROR;
use super::super::generation::GenerationDelegate;
use super::ReconnectInner;
use super::SessionBinding;
use crate::remote_session::wait_for_watch;

#[derive(Clone)]
pub(super) struct OpeningAttempt {
    pub(super) result: watch::Receiver<Option<Result<SessionBinding, String>>>,
    pub(super) unidentified_backend: Arc<AtomicBool>,
}

impl ReconnectInner {
    pub(super) async fn get_or_open_binding(self: &Arc<Self>) -> Result<SessionBinding, String> {
        let receiver = {
            let mut opening = self.opening.lock().unwrap_or_else(PoisonError::into_inner);
            if self.shutdown_requested.is_cancelled() {
                return Err(SHUTDOWN_ERROR.to_string());
            }
            if let Some(attempt) = opening.as_ref()
                && attempt.result.borrow().is_none()
            {
                attempt.result.clone()
            } else {
                if let Some(attempt) = opening.as_ref()
                    && attempt.unidentified_backend.load(Ordering::Acquire)
                    && let Some(Err(error)) = &*attempt.result.borrow()
                {
                    return Err(error.clone());
                }
                if let Some(binding) = self.live_binding() {
                    return Ok(binding);
                }
                let permit = Arc::clone(&self.opening_permit)
                    .try_acquire_owned()
                    .map_err(|_| "gRPC code-mode opening coordinator is unavailable".to_string())?;
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                let (sender, result) = watch::channel(None);
                let unidentified_backend = Arc::new(AtomicBool::new(false));
                *opening = Some(OpeningAttempt {
                    result: result.clone(),
                    unidentified_backend: Arc::clone(&unidentified_backend),
                });
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    let result = inner
                        .open_generation(generation, &unidentified_backend)
                        .await;
                    drop(permit);
                    sender.send_replace(Some(result));
                });
                result
            }
        };
        tokio::select! {
            biased;
            _ = self.shutdown_requested.cancelled() => Err(SHUTDOWN_ERROR.to_string()),
            result = wait_for_watch(receiver) => result,
        }
    }

    async fn open_generation(
        &self,
        generation: u64,
        unidentified_backend: &AtomicBool,
    ) -> Result<SessionBinding, String> {
        let previous = self
            .binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(binding) = previous {
            wait_for_watch(binding.session.inner.request_shutdown()).await?;
        }
        let delegate = Arc::new(GenerationDelegate {
            delegate: Arc::clone(&self.delegate),
            generation,
        });
        let session = self
            .provider
            .open_binding(
                delegate,
                self.limits.clone(),
                unidentified_backend,
                |session| {
                    let mut current = self.binding.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(previous) = current.replace(SessionBinding {
                        session,
                        generation,
                    }) {
                        self.retired
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(previous);
                    }
                    unidentified_backend.store(false, Ordering::Release);
                },
            )
            .await?;
        Ok(SessionBinding {
            session,
            generation,
        })
    }
}
