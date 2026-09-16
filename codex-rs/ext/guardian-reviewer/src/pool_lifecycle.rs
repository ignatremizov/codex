//! Admission, creation and durable retirement are owned independently of review waiters.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use codex_analytics::GuardianReviewSessionKind;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use super::ReviewerPool;
use super::ReviewerSession;
use super::ReviewerTasks;

type Outcome = Result<(), String>;

struct State<S> {
    closed: bool,
    sessions: Vec<Arc<S>>,
    attempt: Option<watch::Receiver<Option<Outcome>>>,
    failure: Option<String>,
}

pub(super) struct Lifecycle<S> {
    state: Mutex<State<S>>,
    openings: TaskTracker,
    reviews: TaskTracker,
}

impl<S> Default for Lifecycle<S> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                closed: false,
                sessions: Vec::new(),
                attempt: None,
                failure: None,
            }),
            openings: TaskTracker::new(),
            reviews: TaskTracker::new(),
        }
    }
}

impl<S: ReviewerSession> Lifecycle<S> {
    pub(super) fn admit(&self) -> anyhow::Result<TaskTrackerToken> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(!state.closed, "Guardian is stopping");
        Ok(self.reviews.token())
    }

    pub(super) async fn shutdown(
        self: &Arc<Self>,
        runtime: Arc<ReviewerTasks>,
    ) -> anyhow::Result<()> {
        let mut receiver = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.closed = true;
            runtime.cancellation.cancel();
            self.openings.close();
            self.reviews.close();
            if let Some(attempt) = state.attempt.as_ref()
                && !matches!(&*attempt.borrow(), Some(Err(_)))
            {
                attempt.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                state.attempt = Some(receiver.clone());
                let lifecycle = Arc::clone(self);
                tokio::spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
                        lifecycle.openings.wait().await;
                        let sessions = {
                            let state = lifecycle
                                .state
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            if let Some(failure) = &state.failure {
                                return Err(failure.clone());
                            }
                            state.sessions.clone()
                        };
                        let mut first_error = None;
                        for session in sessions {
                            match session.shutdown_durably().await {
                                Ok(()) => {
                                    lifecycle
                                        .state
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .sessions
                                        .retain(|retained| !Arc::ptr_eq(retained, &session));
                                }
                                Err(error) => {
                                    first_error.get_or_insert_with(|| format!("{error:#}"));
                                }
                            }
                        }
                        if let Some(error) = first_error {
                            return Err(error);
                        }
                        // Managed lifetimes include partial startup cleanup. Their tracker
                        // must not acknowledge until each exact writer has been released.
                        lifecycle.reviews.wait().await;
                        runtime.tasks.close();
                        runtime.tasks.wait().await;
                        Ok(())
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err(
                            "timed out draining Guardian actors and writers; retry shutdown"
                                .to_string(),
                        )
                    });
                    sender.send_replace(Some(result));
                });
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            receiver.changed().await.map_err(|_| {
                anyhow::anyhow!("Guardian shutdown task ended without acknowledgement")
            })?;
        }
    }
}

impl<S: ReviewerSession> ReviewerPool<S> {
    pub(super) async fn spawn_session(
        &self,
        setup: Arc<S::Setup>,
        context: S::Context,
        kind: GuardianReviewSessionKind,
        snapshot: Option<S::Snapshot>,
        cancellation: CancellationToken,
    ) -> anyhow::Result<Arc<S>> {
        let opening = {
            let state = self
                .lifecycle
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            anyhow::ensure!(!state.closed, "Guardian is stopping");
            self.lifecycle.openings.token()
        };
        let lifecycle = Arc::clone(&self.lifecycle);
        let spawn = Arc::clone(&self.spawn);
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let mut opening = Opening {
                lifecycle: Arc::clone(&lifecycle),
                _token: opening,
                completed: false,
            };
            let result = spawn(setup, context, kind, snapshot, cancellation).await;
            let result = result.map(|session| {
                let session = Arc::new(session);
                let mut state = lifecycle
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                state
                    .sessions
                    .retain(|session| !session.durable_shutdown_complete());
                state.sessions.push(Arc::clone(&session));
                session
            });
            opening.completed = true;
            // Publication precedes handoff: an abandoned waiter cannot lose the actor.
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("Guardian startup task ended without acknowledgement"))?
    }
}

struct Opening<S> {
    lifecycle: Arc<Lifecycle<S>>,
    _token: TaskTrackerToken,
    completed: bool,
}

impl<S> Drop for Opening<S> {
    fn drop(&mut self) {
        if !self.completed {
            self.lifecycle
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .failure =
                Some("Guardian startup task lost its cleanup acknowledgement".to_string());
        }
    }
}

#[cfg(test)]
#[path = "pool_lifecycle_tests.rs"]
mod tests;
