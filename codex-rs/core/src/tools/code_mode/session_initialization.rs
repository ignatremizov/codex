//! Provider creation remains owned after a caller or turn stops waiting.

use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CodeModeSession;
use tokio::sync::watch;

use super::CodeModeService;

type InitializationResult = Result<(), String>;

#[derive(Default)]
pub(super) struct SessionInitialization {
    pub(super) attempt: Mutex<Option<watch::Receiver<Option<InitializationResult>>>>,
}

impl CodeModeService {
    pub(crate) async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        let mut receiver = {
            let mut attempt = self
                .initialization
                .attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.shutdown_token.is_cancelled() {
                return Err("code mode session is shutting down".to_string());
            }
            if let Some(session) = self.session.get() {
                return Ok(Arc::clone(session));
            }
            // A failed compatibility factory returned no cleanup handle. Retrying it
            // could overwrite evidence of an unidentified, partially created backend.
            attempt
                .get_or_insert_with(|| {
                    let (sender, receiver) = watch::channel(None);
                    let provider = Arc::clone(&self.session_provider);
                    let published = Arc::clone(&self.session);
                    let shutdown = self.shutdown_token.clone();
                    tokio::spawn(async move {
                        let result = match provider.create_owned_session().await {
                            Ok(session) => {
                                // Publish before closing so durable shutdown retains this exact
                                // session even if a concurrent fast close fails.
                                let _ = published.set(Arc::clone(&session));
                                if shutdown.is_cancelled() {
                                    let _ = session.shutdown().await;
                                }
                                Ok(())
                            }
                            Err(error) => Err(error),
                        };
                        sender.send_replace(Some(result));
                    });
                    receiver
                })
                .clone()
        };
        loop {
            if self.shutdown_token.is_cancelled() {
                return Err("code mode session is shutting down".to_string());
            }
            if let Some(result) = receiver.borrow_and_update().clone() {
                result?;
                return self.session.get().cloned().ok_or_else(|| {
                    "code mode provider completed without publishing its session".to_string()
                });
            }
            tokio::select! {
                _ = self.shutdown_token.cancelled() => {
                    return Err("code mode session is shutting down".to_string());
                }
                result = receiver.changed() => {
                    result.map_err(|_| "code mode session initialization task failed".to_string())?;
                }
            }
        }
    }
}
