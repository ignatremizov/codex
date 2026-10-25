use super::MIN_INTERVAL;
use super::Reclaimer;
use super::SETTLE_DELAY;
use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn requests_coalesce_and_followup_waits_for_the_process_cooldown() -> Result<()> {
    let reclaimer = Arc::new(Reclaimer::default());
    let calls = Arc::new(AtomicUsize::new(/*v*/ 0));
    let first_calls = Arc::clone(&calls);
    let first = reclaimer
        .schedule(move || {
            first_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
        })
        .context("first request must schedule reclamation")?;
    assert!(reclaimer.schedule(|| {}).is_none());
    tokio::time::advance(SETTLE_DELAY - Duration::from_secs(/*secs*/ 1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
    first.await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let next_calls = Arc::clone(&calls);
    let next = reclaimer
        .schedule(move || {
            next_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
        })
        .context("next request must schedule a delayed pass")?;
    tokio::time::advance(MIN_INTERVAL - Duration::from_secs(/*secs*/ 1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
    next.await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_waiting_pass_releases_admission_without_starting_a_cooldown() -> Result<()> {
    let reclaimer = Arc::new(Reclaimer::default());
    let pending = reclaimer
        .schedule(|| {})
        .context("initial request must be admitted")?;
    pending.abort();
    assert!(pending.await.is_err_and(|error| error.is_cancelled()));

    let calls = Arc::new(AtomicUsize::new(/*v*/ 0));
    let next_calls = Arc::clone(&calls);
    let replacement = reclaimer
        .schedule(move || {
            next_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
        })
        .context("cancellation must not strand the process-wide reservation")?;
    tokio::time::advance(SETTLE_DELAY).await;
    replacement.await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn cancelling_the_waiter_keeps_an_executing_pass_exclusive() -> Result<()> {
    let reclaimer = Arc::new(Reclaimer::default());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let pending = reclaimer
        .schedule(move || {
            let _ = started_tx.send(());
            // Sender drop also releases the worker if an assertion fails.
            let _ = release_rx.recv_timeout(Duration::from_secs(/*secs*/ 30));
        })
        .context("initial pass must be scheduled")?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), started_rx).await??;
    pending.abort();
    assert!(pending.await.is_err_and(|error| error.is_cancelled()));
    assert!(
        reclaimer.schedule(|| {}).is_none(),
        "the detached blocking worker still owns the reservation"
    );
    release_tx.send(())?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            let scheduled = {
                reclaimer
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .scheduled
            };
            if !scheduled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    let next = reclaimer
        .schedule(|| {})
        .context("completed worker must release admission")?;
    next.abort();
    assert!(next.await.is_err_and(|error| error.is_cancelled()));
    Ok(())
}
