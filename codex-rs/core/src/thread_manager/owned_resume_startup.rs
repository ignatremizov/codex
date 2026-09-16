//! Cancels abandoned resume initialization without cancelling ownership publication.
//!
//! Partial startup retains its persistence acquisition and runtime through durable cleanup.
//! Once startup succeeds, the detached resume worker finishes the existing publication
//! transaction even if its caller disconnects.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use super::ThreadManagerState;
use super::ThreadRegistration;
use super::ThreadSpawnRequest;
use super::ThreadSpawnResult;
use crate::session::startup::SessionStartup;

impl ThreadManagerState {
    pub(super) async fn spawn_resumed_thread(
        &self,
        mut request: ThreadSpawnRequest,
        abandoned: CancellationToken,
    ) -> CodexResult<ThreadSpawnResult> {
        debug_assert!(request.registration == ThreadRegistration::Deferred);
        // Keep a supplied managed startup as the acquisition/actor owner, rather than
        // replacing the handle that its outer lifetime task is already retaining.
        let startup = Arc::clone(
            request
                .startup
                .get_or_insert_with(|| Arc::new(SessionStartup::default())),
        );
        // The losing initialization future is dropped before cleanup, releasing its
        // borrows while SessionStartup still owns in-flight persistence acquisition.
        let result = AssertUnwindSafe(async {
            tokio::select! {
                biased;
                _ = abandoned.cancelled() => Err(CodexErr::TurnAborted),
                result = Box::pin(self.spawn_thread(request)) => result,
            }
        })
        .catch_unwind()
        .await
        .unwrap_or(Err(CodexErr::InternalAgentDied));
        if result.is_ok() {
            // The caller's detached publication transaction now owns the runtime,
            // including its existing rollback path if publication fails.
            startup.persistence.lock().await.commit();
        } else {
            // Return the startup error without losing cleanup ownership or making the
            // caller await retries indefinitely. Never retain the entire manager here.
            tokio::spawn(async move {
                loop {
                    match AssertUnwindSafe(startup.cleanup_durably())
                        .catch_unwind()
                        .await
                        .unwrap_or(Err(CodexErr::InternalAgentDied))
                    {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(%error, "abandoned resume cleanup failed; retaining owner");
                            tokio::time::sleep(Duration::from_secs(/*secs*/ 1)).await;
                        }
                    }
                }
            });
        }
        result
    }
}
