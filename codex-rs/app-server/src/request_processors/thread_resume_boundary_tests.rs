use super::*;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn supersession_after_first_drain_event_leaves_remaining_events_for_new_listener() {
    for signal_cancellation in [false, true] {
        let state = Arc::new(Mutex::new(ThreadState::default()));
        state.lock().await.listener_generation = 7;
        let queued = Arc::new(StdMutex::new(VecDeque::from([
            Event {
                id: "first".into(),
                msg: EventMsg::ShutdownComplete,
            },
            Event {
                id: "second".into(),
                msg: EventMsg::ShutdownComplete,
            },
            Event {
                id: "third".into(),
                msg: EventMsg::ShutdownComplete,
            },
        ])));
        let processed = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            let queued = Arc::clone(&queued);
            let processed = Arc::clone(&processed);
            let release = Arc::clone(&release);
            async move {
                let mut entered_tx = Some(entered_tx);
                drain_events_before_resume(
                    /*pending_event_count*/ 3,
                    /*listener_generation*/ 7,
                    &state,
                    &mut cancel_rx,
                    || Ok(queued.lock().expect("queue").pop_front()),
                    |_| {
                        let entered_tx = entered_tx.take();
                        let processed = Arc::clone(&processed);
                        let release = Arc::clone(&release);
                        async move {
                            processed.fetch_add(1, Ordering::SeqCst);
                            if let Some(entered_tx) = entered_tx {
                                entered_tx.send(()).expect("observer");
                            }
                            release.notified().await;
                        }
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .expect("first event reached handler")
            .expect("handler active");
        state.lock().await.listener_generation = 8;
        if signal_cancellation {
            cancel_tx
                .send(())
                .expect("old listener receives cancellation");
        }
        release.notify_one();
        let outcome = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("old drain stops")
            .expect("drain task");
        assert!(matches!(outcome, ResumeDrainOutcome::Interrupted));
        assert_eq!(processed.load(Ordering::SeqCst), 1);
        assert_eq!(
            queued
                .lock()
                .expect("queue")
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "third"],
        );
    }
}
