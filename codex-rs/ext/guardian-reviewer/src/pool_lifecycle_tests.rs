use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use codex_analytics::GuardianReviewAnalyticsResult;
use codex_analytics::GuardianReviewSessionKind;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::super::ReviewSessionResult;
use super::super::ReviewerPool;
use super::super::ReviewerRequest;
use super::super::ReviewerSession;
use super::super::ReviewerTasks;
use super::super::SessionDisposition;
use crate::GuardianReviewSessionOutcome;

#[derive(Default)]
struct Actor {
    attempts: AtomicUsize,
    fail: AtomicBool,
    closed: AtomicBool,
    closing: CancellationToken,
    release: CancellationToken,
}

struct Session {
    key: usize,
    actor: Arc<Actor>,
}

impl ReviewerSession for Session {
    type Setup = ();
    type Context = usize;
    type Snapshot = ();

    fn context(&self) -> &usize {
        &self.key
    }

    async fn snapshot(&self) -> Option<()> {
        None
    }

    async fn commit_snapshot(&self) {}

    async fn shutdown_durably(&self) -> anyhow::Result<()> {
        self.actor.attempts.fetch_add(1, Ordering::SeqCst);
        self.actor.closing.cancel();
        self.actor.release.cancelled().await;
        anyhow::ensure!(
            !self.actor.fail.load(Ordering::SeqCst),
            "writer close failed"
        );
        self.actor.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn durable_shutdown_complete(&self) -> bool {
        self.actor.closed.load(Ordering::SeqCst)
    }
}

struct Request {
    key: usize,
    started: CancellationToken,
    release: CancellationToken,
}

impl ReviewerRequest for Request {
    type Session = Session;

    fn setup(&self) -> Arc<()> {
        Arc::new(())
    }

    fn context(&self, _previous: Option<&Session>) -> usize {
        self.key
    }

    fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 5)
    }

    fn cancellation(&self) -> Option<&CancellationToken> {
        None
    }

    async fn run(
        &self,
        _session: &Session,
        _kind: GuardianReviewSessionKind,
    ) -> ReviewSessionResult {
        self.started.cancel();
        self.release.cancelled().await;
        ReviewSessionResult {
            outcome: GuardianReviewSessionOutcome::Completed(Ok(None)),
            disposition: SessionDisposition::Reusable,
            analytics: GuardianReviewAnalyticsResult::without_session(),
        }
    }
}

#[tokio::test]
async fn dropped_shutdown_waiter_keeps_one_owned_attempt_and_retries_failed_actor() {
    let actor = Arc::new(Actor::default());
    actor.fail.store(true, Ordering::SeqCst);
    let spawn_actor = Arc::clone(&actor);
    let pool = Arc::new(ReviewerPool::new(
        Arc::new(ReviewerTasks::default()),
        move |_, key, _, _, _| {
            let actor = Arc::clone(&spawn_actor);
            Box::pin(async move { Ok(Session { key, actor }) })
        },
    ));
    pool.prewarm(Arc::new(()), /*context*/ 1).await.unwrap();
    let stopping = Arc::clone(&pool);
    let first = tokio::spawn(async move { stopping.shutdown_durably().await });
    actor.closing.cancelled().await;
    first.abort();
    let _ = first.await;
    let mut second = Box::pin(pool.shutdown_durably());
    std::future::poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    actor.release.cancel();
    assert!(second.await.is_err());
    assert_eq!(actor.attempts.load(Ordering::SeqCst), 1);
    actor.fail.store(false, Ordering::SeqCst);
    pool.shutdown_durably().await.unwrap();
    assert_eq!(actor.attempts.load(Ordering::SeqCst), 2);
    assert!(actor.closed.load(Ordering::SeqCst));
    assert!(pool.prewarm(Arc::new(()), /*context*/ 2).await.is_err());
}

#[tokio::test]
async fn abandoned_prewarm_creation_is_drained_before_shutdown_acknowledges() {
    let actor = Arc::new(Actor::default());
    actor.release.cancel();
    let opening = CancellationToken::new();
    let release = CancellationToken::new();
    let spawn_actor = Arc::clone(&actor);
    let spawn_opening = opening.clone();
    let spawn_release = release.clone();
    let runtime = Arc::new(ReviewerTasks::default());
    let pool = Arc::new(ReviewerPool::new(
        Arc::clone(&runtime),
        move |_, key, _, _, _| {
            let actor = Arc::clone(&spawn_actor);
            let opening = spawn_opening.clone();
            let release = spawn_release.clone();
            Box::pin(async move {
                opening.cancel();
                release.cancelled().await;
                Ok(Session { key, actor })
            })
        },
    ));
    let warming = Arc::clone(&pool);
    let warm = tokio::spawn(async move {
        warming.prewarm(Arc::new(()), /*context*/ 1).await
    });
    opening.cancelled().await;
    warm.abort();
    let _ = warm.await;
    let stopping = Arc::clone(&pool);
    let mut shutdown = tokio::spawn(async move { stopping.shutdown_durably().await });
    runtime.cancellation.cancelled().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 20), &mut shutdown)
            .await
            .is_err()
    );
    release.cancel();
    shutdown.await.unwrap().unwrap();
    assert!(actor.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn abandoned_trunk_and_ephemeral_reviews_keep_both_actors_owned() {
    let trunk = Arc::new(Actor::default());
    let fork = Arc::new(Actor::default());
    trunk.release.cancel();
    fork.release.cancel();
    let spawn_trunk = Arc::clone(&trunk);
    let spawn_fork = Arc::clone(&fork);
    let pool = Arc::new(ReviewerPool::new(
        Arc::new(ReviewerTasks::default()),
        move |_, key, kind, _, _| {
            let actor = if matches!(kind, GuardianReviewSessionKind::EphemeralForked) {
                Arc::clone(&spawn_fork)
            } else {
                Arc::clone(&spawn_trunk)
            };
            Box::pin(async move { Ok(Session { key, actor }) })
        },
    ));
    let trunk_started = CancellationToken::new();
    let fork_started = CancellationToken::new();
    let mut reviews = Vec::new();
    for started in [&trunk_started, &fork_started] {
        let pool = Arc::clone(&pool);
        let request = Request {
            key: 1,
            started: started.clone(),
            release: CancellationToken::new(),
        };
        reviews.push(tokio::spawn(async move { pool.review(request).await }));
        started.cancelled().await;
    }
    for review in reviews {
        review.abort();
        let _ = review.await;
    }
    pool.shutdown_durably().await.unwrap();
    assert_eq!(
        (
            trunk.closed.load(Ordering::SeqCst),
            fork.closed.load(Ordering::SeqCst),
        ),
        (true, true),
    );
}

#[tokio::test]
async fn writer_acknowledgement_still_waits_for_accepted_review_callbacks() {
    let actor = Arc::new(Actor::default());
    actor.release.cancel();
    let spawn_actor = Arc::clone(&actor);
    let pool = Arc::new(ReviewerPool::new(
        Arc::new(ReviewerTasks::default()),
        move |_, key, _, _, _| {
            let actor = Arc::clone(&spawn_actor);
            Box::pin(async move { Ok(Session { key, actor }) })
        },
    ));
    let started = CancellationToken::new();
    let release = CancellationToken::new();
    let request = Request {
        key: 1,
        started: started.clone(),
        release: release.clone(),
    };
    let reviewing = Arc::clone(&pool);
    let review = tokio::spawn(async move { reviewing.review(request).await });
    started.cancelled().await;
    let stopping = Arc::clone(&pool);
    let mut shutdown = tokio::spawn(async move { stopping.shutdown_durably().await });
    actor.closing.cancelled().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 20), &mut shutdown)
            .await
            .is_err()
    );
    assert!(actor.closed.load(Ordering::SeqCst));
    release.cancel();
    let _ = review.await.unwrap();
    shutdown.await.unwrap().unwrap();
}

#[tokio::test]
async fn context_replacement_keeps_the_retired_actor_until_writer_acknowledgement() {
    let retired = Arc::new(Actor::default());
    let current = Arc::new(Actor::default());
    retired.release.cancel();
    current.release.cancel();
    let first = Arc::clone(&retired);
    let second = Arc::clone(&current);
    let pool = ReviewerPool::new(
        Arc::new(ReviewerTasks::default()),
        move |_, key, _, _, _| {
            let actor = if key == 1 {
                Arc::clone(&first)
            } else {
                Arc::clone(&second)
            };
            Box::pin(async move { Ok(Session { key, actor }) })
        },
    );
    pool.prewarm(Arc::new(()), /*context*/ 1).await.unwrap();
    let release = CancellationToken::new();
    release.cancel();
    let _ = pool
        .review(Request {
            key: 2,
            started: CancellationToken::new(),
            release,
        })
        .await;
    pool.shutdown_durably().await.unwrap();
    assert_eq!(
        (
            retired.closed.load(Ordering::SeqCst),
            current.closed.load(Ordering::SeqCst),
        ),
        (true, true),
    );
}

#[tokio::test]
async fn lost_creation_cannot_be_mistaken_for_an_empty_pool() {
    let pool = ReviewerPool::<Session>::new(Arc::new(ReviewerTasks::default()), |_, _, _, _, _| {
        Box::pin(async { panic!("lost creation acknowledgement") })
    });
    assert!(pool.prewarm(Arc::new(()), /*context*/ 1).await.is_err());
    assert!(pool.shutdown_durably().await.is_err());
    assert!(pool.shutdown_durably().await.is_err());
}

#[tokio::test]
async fn rejected_creation_without_an_actor_is_not_a_phantom_runtime() {
    let pool = ReviewerPool::<Session>::new(Arc::new(ReviewerTasks::default()), |_, _, _, _, _| {
        Box::pin(async { anyhow::bail!("configuration rejected") })
    });
    assert!(pool.prewarm(Arc::new(()), /*context*/ 1).await.is_err());
    pool.shutdown_durably().await.unwrap();
}
