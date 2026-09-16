//! Retain current, retired, and not-yet-published reviewer runtimes through durable shutdown.

use super::GuardianReviewSession;
use super::GuardianReviewSessionManager;
use super::GuardianReviewSessionState;
use anyhow::anyhow;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use std::future::Future;
use std::sync::Arc;

pub(super) type OwnedReviewSpawn =
    Shared<BoxFuture<'static, Result<Arc<GuardianReviewSession>, SpawnFailure>>>;

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum SpawnFailure {
    #[error("{0:#}")]
    Rejected(Arc<anyhow::Error>),
    #[error("guardian creation task failed: {0}")]
    Lost(Arc<tokio::task::JoinError>),
}

impl GuardianReviewSessionState {
    pub(super) fn prune_acknowledged(&mut self) {
        self.retired_reviews
            .retain(|session| !session.io.durable_shutdown_succeeded());
        self.pending_spawns
            .retain(|spawn| match spawn.clone().now_or_never() {
                Some(Ok(session)) => !session.io.durable_shutdown_succeeded(),
                Some(Err(SpawnFailure::Rejected(_))) => false,
                Some(Err(SpawnFailure::Lost(_))) | None => true,
            });
    }

    pub(super) fn spawn_owned(
        &mut self,
        creation: impl Future<Output = anyhow::Result<GuardianReviewSession>> + Send + 'static,
    ) -> anyhow::Result<OwnedReviewSpawn> {
        if self.closed {
            return Err(anyhow!("guardian session creation is closed for shutdown"));
        }
        self.prune_acknowledged();
        let task = tokio::spawn(creation);
        let result = async move {
            match task.await {
                Ok(result) => result
                    .map(Arc::new)
                    .map_err(|error| SpawnFailure::Rejected(Arc::new(error))),
                Err(error) => Err(SpawnFailure::Lost(Arc::new(error))),
            }
        }
        .boxed()
        .shared();
        // The same lock fences admission and retains the result before any waiter can
        // cancel. A completed, unpublished session remains owned by this shared result.
        self.pending_spawns.push(result.clone());
        Ok(result)
    }
}

impl GuardianReviewSessionManager {
    pub(crate) async fn shutdown_durably(&self) -> anyhow::Result<()> {
        let (spawns, mut sessions) = {
            let mut state = self.state.lock().await;
            state.closed = true;
            self.cancellation_token.cancel();
            let sessions = state
                .trunk
                .iter()
                .chain(&state.ephemeral_reviews)
                .chain(&state.retired_reviews)
                .cloned()
                .collect::<Vec<_>>();
            (state.pending_spawns.clone(), sessions)
        };
        // Never await creation or recursively shut down another session under the
        // manager's selection lock. Complete every cleanup even if another fails.
        let mut failure = None;
        for spawn in spawns {
            match spawn.await {
                Ok(session) => sessions.push(session),
                Err(SpawnFailure::Rejected(_)) => {}
                Err(error @ SpawnFailure::Lost(_)) => {
                    failure.get_or_insert_with(|| anyhow!("guardian creation failed: {error:#}"));
                }
            }
        }
        let mut unique = Vec::<Arc<GuardianReviewSession>>::new();
        for session in sessions {
            if !unique
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &session))
            {
                unique.push(session);
            }
        }
        for session in &unique {
            session.cancel_token.cancel();
            if let Err(error) = session.io.shutdown_durably_and_wait().await {
                failure.get_or_insert_with(|| anyhow!("guardian durable shutdown failed: {error}"));
            }
        }
        let mut state = self.state.lock().await;
        if state
            .trunk
            .as_ref()
            .is_some_and(|session| session.io.durable_shutdown_succeeded())
        {
            state.trunk = None;
        }
        state
            .ephemeral_reviews
            .retain(|session| !session.io.durable_shutdown_succeeded());
        state.prune_acknowledged();
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "review_session_shutdown_tests.rs"]
mod tests;
