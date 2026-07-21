//! Fork-owned compatibility checkpoint for the derived inter-agent transcript projection.
//!
//! Upstream binaries can advance their own checkpoint without projecting these items. The two
//! checkpoints must agree before incremental catch-up is safe, including at canonical EOF.
//! Publication and deletion happen in the same transaction as derived history rows.

use codex_protocol::ThreadId;

use super::super::LocalThreadStore;
use super::sqlite_integer;
use super::thread_history_error;
use crate::ThreadStoreResult;

pub(in crate::local) async fn is_current(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    next_byte_offset: u64,
    next_ordinal: u64,
) -> ThreadStoreResult<bool> {
    let pool = store.thread_history_db().await?;
    ensure_table(pool).await?;
    let actual = sqlx::query_as::<_, (i64, i64)>(
        "SELECT next_rollout_byte_offset, next_rollout_ordinal \
         FROM fork_thread_history_projection_state WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(thread_history_error)?;
    let expected = (
        sqlite_integer(next_byte_offset, "rollout byte offset")?,
        sqlite_integer(next_ordinal, "rollout ordinal")?,
    );
    Ok(actual == Some(expected))
}

pub(super) async fn ensure_table(pool: &sqlx::SqlitePool) -> ThreadStoreResult<()> {
    // Rebuildable fork compatibility state does not claim an upstream migration version.
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS fork_thread_history_projection_state (
    thread_id TEXT PRIMARY KEY,
    next_rollout_byte_offset INTEGER NOT NULL,
    next_rollout_ordinal INTEGER NOT NULL
)
        "#,
    )
    .execute(pool)
    .await
    .map_err(thread_history_error)?;
    Ok(())
}
