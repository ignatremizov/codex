//! External-home entry points never consult SQLite or publish recovered rollout files.
//!
//! The shared readers already enforce journal-qualified, read-only backup access. Keep this
//! boundary on those implementations instead of introducing a weaker backup discovery policy.

use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Locates an active thread without accessing the source home's SQLite database.
pub async fn find_thread_path_by_id_str_without_recovery(
    codex_home: &Path,
    id_str: &str,
) -> io::Result<Option<PathBuf>> {
    crate::find_thread_path_by_id_str(codex_home, id_str, /*state_db_ctx*/ None).await
}

/// Locates an archived thread without accessing the source home's SQLite database.
pub async fn find_archived_thread_path_by_id_str_without_recovery(
    codex_home: &Path,
    id_str: &str,
) -> io::Result<Option<PathBuf>> {
    crate::find_archived_thread_path_by_id_str(codex_home, id_str, /*state_db_ctx*/ None).await
}

/// Opens one retained source representation without writing to its directory.
pub async fn open_rollout_seekable_reader_without_recovery(path: &Path) -> io::Result<File> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::open_rollout_seekable_reader(&path))
        .await
        .map_err(io::Error::other)?
}
