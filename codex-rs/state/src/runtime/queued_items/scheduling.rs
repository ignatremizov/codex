//! Timed input shares the ordinary queue; receipts contain no model output.

use super::*;

impl SqliteQueueStore {
    pub async fn due_loaded_threads(
        &self,
        thread_ids: &[ThreadId],
        now: i64,
    ) -> anyhow::Result<Vec<ThreadId>> {
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT thread_id FROM queued_items WHERE scheduled_at <= ",
        );
        query.push_bind(now).push(" AND thread_id IN (");
        let mut ids = query.separated(", ");
        for id in thread_ids {
            ids.push_bind(id.to_string());
        }
        ids.push_unseparated(")");
        query
            .build_query_scalar::<String>()
            .fetch_all(self.pool.as_ref())
            .await?
            .into_iter()
            .map(|id| ThreadId::try_from(id).map_err(Into::into))
            .collect()
    }

    pub async fn scheduled_threads(
        &self,
        profile: &str,
        now: i64,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT thread_id FROM queued_items
             WHERE fallback_profile = ? AND scheduled_at <= ?
             AND (? IS NULL OR thread_id > ?)
             GROUP BY thread_id ORDER BY thread_id LIMIT ?",
        )
        .bind(profile)
        .bind(now)
        .bind(after)
        .bind(after)
        .bind(i64::try_from(limit)?)
        .fetch_all(self.pool.as_ref())
        .await?;
        ids.into_iter()
            .map(|id| ThreadId::try_from(id).map_err(Into::into))
            .collect()
    }

    pub async fn get_submission(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<QueuedUserSubmissionRecord>> {
        sqlx::query(
            "SELECT id, thread_id, payload_json, scheduled_at, fallback_profile, dispatch_error
             FROM queued_items WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await?
        .as_ref()
        .map(QueuedUserSubmissionRecord::try_from_row)
        .transpose()
    }

    pub async fn set_dispatch_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE queued_items SET dispatch_error = ? WHERE id = ?")
            .bind(error)
            .bind(id)
            .execute_with_busy_retry(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// Publish the input-to-turn association and retire its queue entry together.
    pub async fn acknowledge_submission(
        &self,
        receipt: &QueuedSubmissionReceipt,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin_immediate().await?;
        sqlx::query(
            "INSERT INTO queued_input_receipts(id, thread_id, turn_id, started)
             VALUES(?, ?, ?, ?) ON CONFLICT(id) DO NOTHING",
        )
        .bind(&receipt.id)
        .bind(receipt.thread_id.to_string())
        .bind(&receipt.turn_id)
        .bind(receipt.started)
        .execute(transaction.as_mut())
        .await?;
        sqlx::query("DELETE FROM queued_items WHERE id = ? AND thread_id = ?")
            .bind(&receipt.id)
            .bind(receipt.thread_id.to_string())
            .execute(transaction.as_mut())
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn submission_receipt(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<QueuedSubmissionReceipt>> {
        sqlx::query(
            "SELECT id, thread_id, turn_id, started FROM queued_input_receipts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await?
        .map(|row| {
            Ok(QueuedSubmissionReceipt {
                id: row.try_get("id")?,
                thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
                turn_id: row.try_get("turn_id")?,
                started: row.try_get("started")?,
            })
        })
        .transpose()
    }
}
