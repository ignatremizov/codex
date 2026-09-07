//! Pure grammar for semantic task labels, independent of lifecycle and filesystem paths.

/// Grammar failure classified so callers can retain their own boundary-specific diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskPathValidationError {
    /// The label is neither `/root` nor an absolute label beginning with `/root/`.
    InvalidRoot,
    /// A segment is empty, dot traversal, or contains whitespace, controls, or a backslash.
    InvalidSegment,
}

/// Validate a canonical task label without resolving, normalizing, or claiming it.
///
/// `/root` is grammatically valid; reserving it for Main is the caller's responsibility.
/// Relative resolution, ownership, and uniqueness are deliberately outside this function.
/// Segment case and Unicode are preserved without filesystem interpretation.
pub fn validate_canonical_task_path(task_path: &str) -> Result<(), TaskPathValidationError> {
    if task_path == "/root" {
        return Ok(());
    }
    let Some(segments) = task_path.strip_prefix("/root/") else {
        return Err(TaskPathValidationError::InvalidRoot);
    };
    if segments.split('/').any(|segment| {
        segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
    }) {
        return Err(TaskPathValidationError::InvalidSegment);
    }
    Ok(())
}

#[cfg(test)]
#[path = "task_path_tests.rs"]
mod tests;
