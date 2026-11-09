use super::runtime_with_thread;
use crate::QueuedSubmissionReceipt;
use crate::QueuedSubmissionSchedule;
use crate::StateRuntime;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn due_scan_observes_time_without_rewriting_queue_and_preserves_edits() -> anyhow::Result<()>
{
    let (runtime, thread_id) = runtime_with_thread().await;
    let queue = runtime.thread_queue();
    let schedule = QueuedSubmissionSchedule {
        scheduled_at: 100,
        fallback_profile: "office".into(),
    };
    let item = queue
        .enqueue_with_schedule(thread_id, "input-1", "before", Some(&schedule))
        .await?;
    let version = queue.change_version().await?;
    assert_eq!(
        queue.due_loaded_threads(&[thread_id], /*now*/ 99).await?,
        Vec::new()
    );
    assert_eq!(
        queue.due_loaded_threads(&[thread_id], /*now*/ 100).await?,
        vec![thread_id]
    );
    assert_eq!(queue.change_version().await?, version);
    assert_eq!(
        queue
            .scheduled_threads(
                "default", /*now*/ 100, /*after*/ None, /*limit*/ 10
            )
            .await?,
        Vec::new()
    );
    assert_eq!(
        queue
            .scheduled_threads(
                "office", /*now*/ 100, /*after*/ None, /*limit*/ 10
            )
            .await?,
        vec![thread_id]
    );
    let mut expected = item;
    expected.payload = "after".into();
    assert_eq!(
        queue.update(thread_id, &expected.id, "after").await?,
        Some(expected)
    );
    Ok(())
}

#[tokio::test]
async fn due_targets_page_past_failed_entries_and_receipts_survive_reopen() -> anyhow::Result<()> {
    let (runtime, first_id) = runtime_with_thread().await;
    let queue = runtime.thread_queue();
    let second_id = ThreadId::new();
    let schedule = QueuedSubmissionSchedule {
        scheduled_at: 1,
        fallback_profile: "office".into(),
    };
    queue
        .enqueue_with_schedule(first_id, "first", "one", Some(&schedule))
        .await?;
    queue
        .enqueue_with_schedule(second_id, "second", "two", Some(&schedule))
        .await?;
    let mut ids = [first_id, second_id];
    ids.sort_by_key(ToString::to_string);
    assert_eq!(
        queue
            .scheduled_threads(
                "office", /*now*/ 1, /*after*/ None, /*limit*/ 1
            )
            .await?,
        vec![ids[0]]
    );
    assert_eq!(
        queue
            .scheduled_threads(
                "office",
                /*now*/ 1,
                Some(&ids[0].to_string()),
                /*limit*/ 1
            )
            .await?,
        vec![ids[1]]
    );
    let receipt = QueuedSubmissionReceipt {
        id: "first".into(),
        thread_id: first_id,
        turn_id: "receiving-turn".into(),
        started: false,
    };
    queue.acknowledge_submission(&receipt).await?;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string()).await?;
    assert_eq!(other.thread_queue().get_submission("first").await?, None);
    assert_eq!(
        other.thread_queue().submission_receipt("first").await?,
        Some(receipt)
    );
    assert!(
        queue
            .enqueue_with_schedule(first_id, "first", "repeat", Some(&schedule))
            .await
            .is_err()
    );
    queue.delete_thread_queue(first_id).await?;
    assert_eq!(queue.submission_receipt("first").await?, None);
    Ok(())
}
