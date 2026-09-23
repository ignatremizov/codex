//! Non-destructive thread deletion preflight and explicitly authorized board deletion.
//!
//! Thread deletion cannot prove exclusive ownership of shared board evidence.
//! Authorized board deletion keeps a tombstone to reject delayed writes.
//! Neither operation recovers corruption by replacing the shared database.

use super::DATABASE_FILE;
use super::LocalAgentMessageBoard;
use super::SCHEMA;
use super::invalid;
use super::storage_error;
use codex_protocol::SessionId;
use codex_protocol::error::Result;
use codex_state::SqliteConfig;
use sqlx::Sqlite;
use sqlx::Transaction;

impl LocalAgentMessageBoard {
    /// Refuses thread deletion if it would require disposing of populated boards.
    ///
    /// Spawn ancestry alone does not account for adopted members or concurrent creation.
    /// Without a fenced ownership proof, preserve all board data, including when the feature
    /// or state DB is unavailable. Empty or missing boards need no cleanup: leave them
    /// untombstoned so concurrent late writes cannot lose evidence.
    pub async fn ensure_thread_deletion_preserves_boards(
        sqlite: &SqliteConfig,
        roots: &[SessionId],
    ) -> Result<()> {
        let path = sqlite.home().join(DATABASE_FILE);
        if roots.is_empty() || !tokio::fs::try_exists(&path).await? {
            return Ok(());
        }
        let pool = sqlite
            .open_read_only_pool(&path, /*busy_timeout*/ None)
            .await
            .map_err(storage_error)?;
        let result = async {
            for root in roots {
                let populated: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM channels WHERE board=?)
                         OR EXISTS(SELECT 1 FROM posts WHERE board=?)
                         OR EXISTS(SELECT 1 FROM subscriptions WHERE board=?)",
                )
                .bind(root.to_string())
                .bind(root.to_string())
                .bind(root.to_string())
                .fetch_one(&pool)
                .await
                .map_err(storage_error)?;
                if populated {
                    return Err(invalid(format!(
                        "cannot delete thread {root}: its message board contains shared history; exclusive cleanup ownership cannot be established"
                    )));
                }
            }
            Ok(())
        }
        .await;
        // Close only this private read-only pool, including on query failure.
        pool.close().await;
        result
    }

    /// Permanently removes boards owned by these roots, including their posts and subscriptions.
    /// A child's ID does not match its parent's board. Unload and archive must not call this.
    /// Callers must first prove exclusive disposal authority and fence new membership.
    /// Ordinary thread deletion must use `ensure_thread_deletion_preserves_boards` instead.
    /// Safe to retry and independent of whether the feature is currently enabled.
    pub async fn delete_boards(sqlite: &SqliteConfig, roots: &[SessionId]) -> Result<()> {
        let path = sqlite.home().join(DATABASE_FILE);
        if roots.is_empty() || !tokio::fs::try_exists(&path).await? {
            return Ok(());
        }
        let pool = sqlite
            .open_read_write_pool(&path)
            .await
            .map_err(storage_error)?;
        // Also handles databases created before permanent deletion was supported.
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .map_err(storage_error)?;
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        for root in roots {
            sqlx::query("INSERT OR IGNORE INTO deleted_boards (board) VALUES (?)")
                .bind(root.to_string())
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            for statement in [
                "DELETE FROM subscriptions WHERE board=?",
                "DELETE FROM posts WHERE board=?",
                "DELETE FROM channels WHERE board=?",
            ] {
                sqlx::query(statement)
                    .bind(root.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        pool.close().await;
        Ok(())
    }

    pub(super) async fn begin_write(&self) -> Result<Transaction<'_, Sqlite>> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        let deleted: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deleted_boards WHERE board=?)")
                .bind(self.identity.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if deleted {
            return Err(invalid(
                "the message board's root has been permanently deleted",
            ));
        }
        Ok(tx)
    }
}
