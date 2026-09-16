//! Durable shutdown owns its attempt independently of the requesting waiter.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tokio::sync::watch;

use super::CodeModeService;

type Outcome = Result<(), String>;

#[derive(Default)]
pub(super) struct DurableShutdown {
    attempt: Mutex<Option<watch::Receiver<Option<Outcome>>>>,
    provider_closed: Arc<AtomicBool>,
}

impl CodeModeService {
    pub(crate) async fn shutdown_durably(&self) -> Outcome {
        let initialization = {
            let initialization = self
                .initialization
                .attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.shutdown_token.cancel();
            initialization.clone()
        };
        self.dispatch_broker.begin_durable_shutdown();
        let published = Arc::clone(&self.session);
        let mut receiver = {
            let mut attempt = self
                .durable_shutdown
                .attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if attempt
                .as_ref()
                .is_some_and(|receiver| matches!(&*receiver.borrow(), Some(Err(_))))
            {
                *attempt = None;
            }
            attempt
                .get_or_insert_with(|| {
                    let (sender, receiver) = watch::channel(None);
                    let broker = Arc::clone(&self.dispatch_broker);
                    let provider_closed = Arc::clone(&self.durable_shutdown.provider_closed);
                    tokio::spawn(async move {
                        let close = async {
                            if provider_closed.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            if let Some(mut initialization) = initialization {
                                loop {
                                    if let Some(result) = initialization.borrow_and_update().clone()
                                    {
                                        result?;
                                        break;
                                    }
                                    initialization.changed().await.map_err(|_| {
                                        "code mode session initialization task failed".to_string()
                                    })?;
                                }
                            }
                            let result = match published.get() {
                                Some(session) => session.shutdown_durably().await,
                                None => Ok(()),
                            };
                            if result.is_ok() {
                                provider_closed.store(true, Ordering::Release);
                            }
                            result
                        };
                        let (result, dispatch_result) =
                            tokio::join!(close, broker.wait_for_accepted_dispatch());
                        sender.send_replace(Some(result.and(dispatch_result)));
                    });
                    receiver
                })
                .clone()
        };
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            receiver.changed().await.map_err(|_| {
                "code mode durable shutdown task ended without a result".to_string()
            })?;
        }
    }
}

#[cfg(test)]
#[path = "durable_shutdown_tests.rs"]
mod tests;
