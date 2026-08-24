use crate::resume_picker::SessionTarget;
use codex_protocol::ThreadId;
use codex_rollout::find_archived_thread_path_by_id_str_without_recovery;
use codex_rollout::find_thread_path_by_id_str_without_recovery;
use codex_rollout::read_session_meta_line;
use codex_utils_absolute_path::canonicalize_existing_preserving_symlinks;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use std::path::Path;

pub(crate) async fn lookup_in_source_home(
    source_home: &Path,
    id_str: &str,
) -> Result<Option<SessionTarget>> {
    let source_home =
        canonicalize_existing_preserving_symlinks(source_home).wrap_err_with(|| {
            format!(
                "failed to resolve fork source home {}",
                source_home.display()
            )
        })?;
    if !source_home.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "fork source home is not a directory: {}",
            source_home.display()
        ));
    }
    let path =
        match find_thread_path_by_id_str_without_recovery(source_home.as_path(), id_str).await? {
            Some(path) => Some(path),
            None => {
                find_archived_thread_path_by_id_str_without_recovery(source_home.as_path(), id_str)
                    .await?
            }
        };
    match path {
        Some(path) => from_rollout_path(path.as_path(), Some(id_str)).await,
        None => Ok(None),
    }
}

pub(crate) async fn from_rollout_path(
    path: &Path,
    expected_id: Option<&str>,
) -> Result<Option<SessionTarget>> {
    let path = codex_rollout::existing_rollout_path(path)
        .await
        .unwrap_or_else(|| path.to_path_buf());
    let path = match canonicalize_existing_preserving_symlinks(path.as_path()) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| {
                color_eyre::eyre::eyre!("fork source rollout has no parent directory")
            })?;
            let parent = canonicalize_existing_preserving_symlinks(parent).wrap_err_with(|| {
                format!(
                    "failed to resolve fork source rollout parent {}",
                    parent.display()
                )
            })?;
            let file_name = path
                .file_name()
                .ok_or_else(|| color_eyre::eyre::eyre!("fork source rollout has no file name"))?;
            parent.join(file_name)
        }
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!("failed to resolve fork source rollout {}", path.display())
            });
        }
    };
    match tokio::fs::metadata(path.as_path()).await {
        Ok(metadata) if !metadata.is_file() => {
            return Err(color_eyre::eyre::eyre!(
                "fork source rollout is not a file: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!("failed to inspect fork source rollout {}", path.display())
            });
        }
    }
    let metadata = read_session_meta_line(path.as_path()).await.ok();
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let thread_id = metadata.meta.id;
    if let Some(expected_id) = expected_id
        && ThreadId::from_string(expected_id).ok() != Some(thread_id)
    {
        return Ok(None);
    }
    Ok(Some(SessionTarget {
        path: Some(path.clone()),
        source_rollout_path: Some(path),
        thread_id,
        cwd: Some(metadata.meta.cwd),
        history_mode: Some(metadata.meta.history_mode),
    }))
}

#[cfg(test)]
#[path = "fork_source_lookup_tests.rs"]
mod tests;
