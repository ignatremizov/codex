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
                msg: EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                    message: "queued event".into(),
                }),
            },
            Event {
                id: "second".into(),
                msg: EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                    message: "queued event".into(),
                }),
            },
            Event {
                id: "third".into(),
                msg: EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                    message: "queued event".into(),
                }),
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

#[tokio::test]
async fn shutdown_during_resume_drain_settles_handler_and_preserves_remaining_events() {
    let state = Mutex::new(ThreadState::default());
    state.lock().await.listener_generation = 7;
    let mut queued = VecDeque::from([
        Event {
            id: "shutdown".into(),
            msg: EventMsg::ShutdownComplete,
        },
        Event {
            id: "later".into(),
            msg: EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                message: "later event".into(),
            }),
        },
    ]);
    let processed = AtomicUsize::new(0);
    let (_cancel_tx, mut cancel_rx) = oneshot::channel();
    let outcome = drain_events_before_resume(
        /*pending_event_count*/ 2,
        /*listener_generation*/ 7,
        &state,
        &mut cancel_rx,
        || Ok(queued.pop_front()),
        |_| async {
            processed.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            processed.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(matches!(outcome, ResumeDrainOutcome::ShutdownComplete));
    assert_eq!(processed.load(Ordering::SeqCst), 2);
    assert_eq!(
        queued
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["later"],
    );
}
