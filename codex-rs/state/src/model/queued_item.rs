use anyhow::Result;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// One durable, ordered user submission for a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedUserSubmissionRecord {
    pub id: String,
    pub thread_id: ThreadId,
    pub payload: String,
    pub schedule: Option<QueuedSubmissionSchedule>,
    pub dispatch_error: Option<String>,
}

/// Due time and credential profile used only when a queued target must be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSubmissionSchedule {
    pub scheduled_at: i64,
    pub fallback_profile: String,
}

/// Associates consumed scheduled input with the ordinary turn that received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSubmissionReceipt {
    pub id: String,
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub started: bool,
}

impl QueuedUserSubmissionRecord {
    pub(crate) fn try_from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
            payload: row.try_get("payload_json")?,
            schedule: row
                .try_get::<Option<i64>, _>("scheduled_at")?
                .map(|scheduled_at| {
                    Ok::<_, sqlx::Error>(QueuedSubmissionSchedule {
                        scheduled_at,
                        fallback_profile: row.try_get("fallback_profile")?,
                    })
                })
                .transpose()?,
            dispatch_error: row.try_get("dispatch_error")?,
        })
    }
}
