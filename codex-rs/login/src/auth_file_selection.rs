use codex_config::AbsolutePathBuf;
use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// The credential-file selection captured when a runtime starts.
///
/// An explicit selection uses file storage instead of a persistent keyring.
/// Ephemeral credentials retain their normal precedence, but are scoped to
/// this selection. The default selection preserves the configured backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthFileSelection {
    #[default]
    Default,
    Selected(AbsolutePathBuf),
}

impl AuthFileSelection {
    /// Capture the process selector at a runtime entry point.
    ///
    /// Retain the returned value for configuration reloads and credential
    /// operations instead of consulting the environment again.
    pub fn from_env(codex_home: &Path) -> io::Result<Self> {
        let selector = std::env::var_os("CODEX_AUTH_FILE");
        Self::resolve(codex_home, selector.as_deref())
    }

    /// Resolve a startup selector without reading credential contents.
    ///
    /// Relative selectors must be a single filename under `codex_home`.
    /// Absolute selectors name a host-local file, not an executor resource.
    pub fn resolve(codex_home: &Path, selector: Option<&OsStr>) -> io::Result<Self> {
        let Some(selector) = selector else {
            return Ok(Self::Default);
        };
        let path = Path::new(selector);
        if selector.is_empty()
            || selector.to_string_lossy().chars().any(char::is_control)
            || (!path.is_absolute() && path.file_name() != Some(selector))
            || path.file_name().is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_AUTH_FILE must be a filename or an absolute file path",
            ));
        }
        let absolute = if path.is_absolute() {
            AbsolutePathBuf::from_absolute_path_checked(path)?
        } else {
            let home = AbsolutePathBuf::from_absolute_path_checked(codex_home)?;
            home.join(path)
        };
        match std::fs::symlink_metadata(absolute.as_path()) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CODEX_AUTH_FILE must not name a directory or symlink",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self::Selected(absolute))
    }
}

#[cfg(test)]
#[path = "auth_file_selection_tests.rs"]
mod tests;
