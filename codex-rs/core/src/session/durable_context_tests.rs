use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use codex_core_skills::SkillInstructions;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::TurnInputContribution;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_rollout::RolloutRecorder;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::tests::attach_thread_persistence;
use super::tests::make_session_and_context;

const FIRST_MARKER: &str = "DURABLE_CONTEXT_FIRST_MARKER";
const SECOND_MARKER: &str = "DURABLE_CONTEXT_SECOND_MARKER";
const NO_MARKERS: MarkerPresence = MarkerPresence {
    first: false,
    second: false,
};
const COMPLETE_BATCH: MarkerPresence = MarkerPresence {
    first: true,
    second: true,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MarkerPresence {
    first: bool,
    second: bool,
}

#[tokio::test]
async fn durable_context_ack_observes_the_complete_rollout_and_history_batch() {
    let (mut session, turn_context) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let session = Arc::new(session);
    let session_at_ack = Arc::clone(&session);
    let rollout_path_at_ack = rollout_path.clone();
    let acknowledgement_count = Arc::new(AtomicUsize::new(0));
    let acknowledgement_count_at_ack = Arc::clone(&acknowledgement_count);
    let (ack_tx, ack_rx) = oneshot::channel();
    let (items, acknowledgement) = contribution(move || {
        acknowledgement_count_at_ack.fetch_add(1, Ordering::SeqCst);
        let history = session_at_ack
            .state
            .try_lock()
            .expect("history should be unlocked before acknowledgement");
        let history_text = response_item_text(history.history.raw_items());
        let rollout_text =
            std::fs::read_to_string(&rollout_path_at_ack).expect("read durable rollout at ack");
        ack_tx
            .send((
                marker_presence(&history_text),
                marker_presence(&rollout_text),
            ))
            .expect("record acknowledgement observation");
    });

    session
        .record_durable_context_items(Arc::new(turn_context), items, acknowledgement)
        .await
        .expect("record complete durable contribution");

    assert_eq!(
        (COMPLETE_BATCH, COMPLETE_BATCH),
        ack_rx.await.expect("receive acknowledgement observation")
    );
    assert_eq!(1, acknowledgement_count.load(Ordering::SeqCst));
    assert_eq!(
        COMPLETE_BATCH,
        persisted_marker_presence(&rollout_path).await
    );
}

#[tokio::test]
async fn durable_context_rollout_failure_records_nothing_and_does_not_acknowledge() {
    let (mut session, turn_context) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    session
        .live_thread()
        .expect("attached live thread")
        .shutdown()
        .await
        .expect("shut down rollout writer");
    let session = Arc::new(session);
    let acknowledgement_count = Arc::new(AtomicUsize::new(0));
    let acknowledgement_count_at_ack = Arc::clone(&acknowledgement_count);
    let (items, acknowledgement) = contribution(move || {
        acknowledgement_count_at_ack.fetch_add(1, Ordering::SeqCst);
    });

    session
        .record_durable_context_items(Arc::new(turn_context), items, acknowledgement)
        .await
        .expect_err("closed rollout writer should reject the complete contribution");

    assert_eq!(0, acknowledgement_count.load(Ordering::SeqCst));
    let history = session.clone_history().await.into_raw_items();
    assert_eq!(NO_MARKERS, marker_presence(&response_item_text(&history)));
    assert_eq!(NO_MARKERS, persisted_marker_presence(&rollout_path).await);
}

#[tokio::test]
async fn cancelling_durable_context_caller_does_not_cancel_the_detached_commit() {
    let (mut session, turn_context) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let session = Arc::new(session);
    let durability_permit = session
        .durable_context_lock
        .acquire()
        .await
        .expect("acquire durability test permit");
    let acknowledgement_count = Arc::new(AtomicUsize::new(0));
    let acknowledgement_count_at_ack = Arc::clone(&acknowledgement_count);
    let (ack_tx, ack_rx) = oneshot::channel();
    let (items, acknowledgement) = contribution(move || {
        acknowledgement_count_at_ack.fetch_add(1, Ordering::SeqCst);
        ack_tx.send(()).expect("record detached acknowledgement");
    });
    let mut caller = Box::pin(session.record_durable_context_items(
        Arc::new(turn_context),
        items,
        acknowledgement,
    ));

    // Poll through the detached spawn and up to its JoinHandle. The held permit makes completion
    // impossible, so dropping this pending future deterministically cancels only the caller.
    std::future::poll_fn(|cx| match caller.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => {
            panic!("durable context caller completed while persistence was locked: {result:?}")
        }
    })
    .await;
    drop(caller);
    assert_eq!(0, acknowledgement_count.load(Ordering::SeqCst));
    let history = session.clone_history().await.into_raw_items();
    assert_eq!(NO_MARKERS, marker_presence(&response_item_text(&history)));
    drop(durability_permit);

    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), ack_rx)
        .await
        .expect("detached commit should finish after caller cancellation")
        .expect("receive detached acknowledgement");
    assert_eq!(1, acknowledgement_count.load(Ordering::SeqCst));
    let history = session.clone_history().await.into_raw_items();
    assert_eq!(
        COMPLETE_BATCH,
        marker_presence(&response_item_text(&history))
    );
    assert_eq!(
        COMPLETE_BATCH,
        persisted_marker_presence(&rollout_path).await
    );
}

fn contribution(
    acknowledgement: impl FnOnce() + Send + 'static,
) -> (
    Vec<ResponseItem>,
    Option<codex_extension_api::TurnInputContributionAcknowledgement>,
) {
    let contribution = TurnInputContribution::with_acknowledgement(
        vec![
            Box::new(SkillInstructions::new(
                "durable-first",
                "/skills/durable-first/SKILL.md",
                FIRST_MARKER,
            )),
            Box::new(SkillInstructions::new(
                "durable-second",
                "/skills/durable-second/SKILL.md",
                SECOND_MARKER,
            )),
        ],
        acknowledgement,
    );
    let (fragments, acknowledgement) = contribution.into_parts();
    (
        fragments
            .into_iter()
            .map(ContextualUserFragment::into_boxed_response_item)
            .collect(),
        acknowledgement,
    )
}

fn response_item_text<'a>(items: impl IntoIterator<Item = &'a ResponseItem>) -> String {
    items
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            ContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn persisted_marker_presence(rollout_path: &Path) -> MarkerPresence {
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(rollout_path)
        .await
        .expect("load persisted durable context")
    else {
        panic!("durable context rollout should have resumed history");
    };
    let response_items = resumed.history.iter().filter_map(|item| match item {
        RolloutItem::ResponseItem(item) => Some(item),
        _ => None,
    });
    marker_presence(&response_item_text(response_items))
}

fn marker_presence(text: &str) -> MarkerPresence {
    MarkerPresence {
        first: text.contains(FIRST_MARKER),
        second: text.contains(SECOND_MARKER),
    }
}
