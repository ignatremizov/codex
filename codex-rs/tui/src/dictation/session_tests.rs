use super::super::ordered::OrderedTranscript;
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cancellation_does_not_release_a_still_owned_microphone() {
    let reservation = MicReservation::default();
    let lease = reservation.acquire().expect("first owner");
    let capture_owner = lease.clone();
    let cancel = CancellationToken::new();
    cancel.cancel();
    drop(lease);
    assert!(reservation.acquire().is_none());
    let (exit, exited) = tokio::sync::oneshot::channel();
    let owner = tokio::spawn(async move {
        let _ = exited.await;
        drop(capture_owner);
    });
    assert!(reservation.acquire().is_none());
    exit.send(()).expect("owner alive");
    owner.await.expect("owner exit");
    assert!(reservation.acquire().is_some());
}

#[tokio::test]
async fn queued_realtime_retry_can_be_canceled_without_acquiring_after_release() {
    let reservation = MicReservation::default();
    let owner = reservation.acquire().expect("old helper");
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let waiter = futures::future::Abortable::new(reservation.acquire_after_release(), registration);
    abort.abort();
    assert!(waiter.await.is_err());
    assert!(reservation.acquire().is_none());
    drop(owner);
    assert!(reservation.acquire().is_some());
}

#[test]
fn completed_out_of_order_results_keep_all_five_reservations() {
    let slots = Arc::new(Semaphore::new(/*permits*/ 5));
    let mut reservations = (0_u64..5)
        .map(|sequence| {
            (
                sequence,
                slots.clone().try_acquire_owned().expect("reserved"),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut ordered = OrderedTranscript::default();
    for sequence in 1..5 {
        assert!(
            ordered
                .resolve(sequence, Ok(format!("chunk {sequence}")))
                .is_empty()
        );
        assert!(slots.clone().try_acquire_owned().is_err());
    }
    let mut queued_ui_events = Vec::new();
    for resolved in ordered.resolve(0, Err("first request failed".into())) {
        queued_ui_events.push(Update::Chunk {
            result: resolved.result,
            reservation: reservations
                .remove(&resolved.sequence)
                .expect("reservation"),
        });
    }
    assert_eq!(queued_ui_events.len(), 5);
    assert_eq!(slots.available_permits(), 0);
    drop(queued_ui_events);
    assert_eq!(slots.available_permits(), 5);
}
