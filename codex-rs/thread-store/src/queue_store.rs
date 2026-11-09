use std::fmt::Display;
use std::future::Future;

use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_state::QueuedSubmissionReceipt;
use codex_state::QueuedSubmissionSchedule;
use codex_state::QueuedUserSubmissionRecord;
use codex_state::SqliteQueueStore;

use crate::MAX_QUEUE_ITEMS;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;

/// Storage-neutral persistence for ordered, thread-scoped user messages.
pub trait QueueStore: Send + Sync {
    /// Return a stable revision that changes when another connection updates the queue.
    fn change_version(&self) -> ThreadStoreFuture<'_, i64>;

    /// Return changed, loaded thread IDs and their durable revisions after `revision`.
    fn changes_since<'a>(
        &'a self,
        revision: i64,
        thread_ids: &'a [ThreadId],
    ) -> ThreadStoreFuture<'a, Vec<(ThreadId, i64)>>;

    fn enqueue(
        &self,
        thread_id: ThreadId,
        payload: String,
    ) -> ThreadStoreFuture<'_, QueuedUserSubmissionRecord>;

    /// Add timed input without exposing a partially configured ordinary entry.
    fn enqueue_scheduled(
        &self,
        thread_id: ThreadId,
        id: String,
        payload: String,
        schedule: QueuedSubmissionSchedule,
    ) -> ThreadStoreFuture<'_, QueuedUserSubmissionRecord>;

    /// Return bounded due targets eligible for cold loading by this profile.
    fn scheduled_threads(
        &self,
        profile: String,
        now: i64,
        after: Option<String>,
        limit: usize,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>>;

    /// Find due input for loaded owners even without a database revision change.
    fn due_loaded_threads(
        &self,
        thread_ids: Vec<ThreadId>,
        now: i64,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>>;

    /// Read pending input by its stable queue identity without loading a runtime.
    fn get_submission(
        &self,
        id: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedUserSubmissionRecord>>;

    /// Record an actionable dispatch error on the pending input.
    fn set_dispatch_error(&self, id: String, error: String) -> ThreadStoreFuture<'_, ()>;

    /// Save the receiving turn identity and retire its pending input together.
    fn acknowledge_submission(&self, receipt: QueuedSubmissionReceipt)
    -> ThreadStoreFuture<'_, ()>;

    /// Read the receiving turn association for consumed input.
    fn submission_receipt(
        &self,
        id: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedSubmissionReceipt>>;

    fn list_page(
        &self,
        thread_id: ThreadId,
        offset: usize,
        limit: usize,
    ) -> ThreadStoreFuture<'_, Vec<QueuedUserSubmissionRecord>>;

    fn update(
        &self,
        thread_id: ThreadId,
        item_id: String,
        payload: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedUserSubmissionRecord>>;

    fn delete(&self, thread_id: ThreadId, item_id: String) -> ThreadStoreFuture<'_, bool>;

    /// Atomically replace queue order with every current item ID exactly once.
    ///
    /// Returns [`ThreadStoreError::InvalidRequest`] when `item_ids` is not a
    /// permutation of the complete queue.
    fn reorder(&self, thread_id: ThreadId, item_ids: Vec<String>) -> ThreadStoreFuture<'_, ()>;
}

/// Adapts the local state runtime to the shared queue-storage interface.
#[derive(Clone)]
pub struct LocalQueueStore {
    state_db: StateDbHandle,
}

impl LocalQueueStore {
    pub fn new(state_db: StateDbHandle) -> Self {
        Self { state_db }
    }

    fn queue(&self) -> &SqliteQueueStore {
        self.state_db.thread_queue()
    }
}

fn queue_future<'a, T, E>(
    future: impl Future<Output = Result<T, E>> + Send + 'a,
) -> ThreadStoreFuture<'a, T>
where
    T: Send + 'a,
    E: Display + Send + 'a,
{
    Box::pin(async move {
        future.await.map_err(|error| ThreadStoreError::Internal {
            message: format!("queue storage failed: {error}"),
        })
    })
}

impl QueueStore for LocalQueueStore {
    fn due_loaded_threads(
        &self,
        thread_ids: Vec<ThreadId>,
        now: i64,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        queue_future(async move { self.queue().due_loaded_threads(&thread_ids, now).await })
    }

    fn enqueue_scheduled(
        &self,
        thread_id: ThreadId,
        id: String,
        payload: String,
        schedule: QueuedSubmissionSchedule,
    ) -> ThreadStoreFuture<'_, QueuedUserSubmissionRecord> {
        Box::pin(async move {
            self.queue()
                .enqueue_with_schedule(thread_id, &id, &payload, Some(&schedule))
                .await
                .map_err(|error| match error.downcast_ref::<sqlx::Error>() {
                    Some(sqlx::Error::RowNotFound) => ThreadStoreError::InvalidRequest {
                        message: format!(
                            "queue is full (limit {MAX_QUEUE_ITEMS}) or input {id} has already been admitted"
                        ),
                    },
                    _ => ThreadStoreError::Internal {
                        message: format!("queue storage failed: {error}"),
                    },
                })
        })
    }

    fn scheduled_threads(
        &self,
        profile: String,
        now: i64,
        after: Option<String>,
        limit: usize,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        queue_future(async move {
            self.queue()
                .scheduled_threads(&profile, now, after.as_deref(), limit)
                .await
        })
    }

    fn get_submission(
        &self,
        id: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedUserSubmissionRecord>> {
        queue_future(async move { self.queue().get_submission(&id).await })
    }

    fn set_dispatch_error(&self, id: String, error: String) -> ThreadStoreFuture<'_, ()> {
        queue_future(async move { self.queue().set_dispatch_error(&id, &error).await })
    }

    fn acknowledge_submission(
        &self,
        receipt: QueuedSubmissionReceipt,
    ) -> ThreadStoreFuture<'_, ()> {
        queue_future(async move { self.queue().acknowledge_submission(&receipt).await })
    }

    fn submission_receipt(
        &self,
        id: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedSubmissionReceipt>> {
        queue_future(async move { self.queue().submission_receipt(&id).await })
    }

    fn change_version(&self) -> ThreadStoreFuture<'_, i64> {
        queue_future(self.queue().change_version())
    }

    fn changes_since<'a>(
        &'a self,
        revision: i64,
        thread_ids: &'a [ThreadId],
    ) -> ThreadStoreFuture<'a, Vec<(ThreadId, i64)>> {
        queue_future(self.queue().changes_since(revision, thread_ids))
    }

    fn enqueue(
        &self,
        thread_id: ThreadId,
        payload: String,
    ) -> ThreadStoreFuture<'_, QueuedUserSubmissionRecord> {
        Box::pin(async move {
            self.queue()
                .enqueue(thread_id, &payload)
                .await
                .map_err(|error| match error.downcast_ref::<sqlx::Error>() {
                    Some(sqlx::Error::RowNotFound) => ThreadStoreError::InvalidRequest {
                        message: format!(
                            "queue cannot contain more than {MAX_QUEUE_ITEMS} submissions"
                        ),
                    },
                    _ => ThreadStoreError::Internal {
                        message: format!("queue storage failed: {error}"),
                    },
                })
        })
    }

    fn list_page(
        &self,
        thread_id: ThreadId,
        offset: usize,
        limit: usize,
    ) -> ThreadStoreFuture<'_, Vec<QueuedUserSubmissionRecord>> {
        queue_future(self.queue().list_page(thread_id, offset, limit))
    }

    fn update(
        &self,
        thread_id: ThreadId,
        item_id: String,
        payload: String,
    ) -> ThreadStoreFuture<'_, Option<QueuedUserSubmissionRecord>> {
        queue_future(async move { self.queue().update(thread_id, &item_id, &payload).await })
    }

    fn delete(&self, thread_id: ThreadId, item_id: String) -> ThreadStoreFuture<'_, bool> {
        queue_future(async move { self.queue().delete(thread_id, &item_id).await })
    }

    fn reorder(&self, thread_id: ThreadId, item_ids: Vec<String>) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.queue()
                .reorder(thread_id, &item_ids)
                .await
                .map_err(|error| match error.downcast_ref::<std::io::Error>() {
                    Some(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                        ThreadStoreError::InvalidRequest {
                            message: error.to_string(),
                        }
                    }
                    _ => ThreadStoreError::Internal {
                        message: format!("queue storage failed: {error}"),
                    },
                })
        })
    }
}
