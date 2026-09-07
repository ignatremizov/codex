use pretty_assertions::assert_eq;

use super::TaskPathValidationError;
use super::validate_canonical_task_path;

#[test]
fn accepts_canonical_labels_without_filesystem_or_lifecycle_restrictions() {
    for task_path in [
        "/root",
        "/root/backend/auth",
        "/root/Backend/認証",
        "/root/Équipe/e\u{301}",
        "/root/.review/.../auth-2",
        "/root/review:contracts/@owner",
    ] {
        assert_eq!(
            validate_canonical_task_path(task_path),
            Ok(()),
            "{task_path:?}"
        );
    }
}

#[test]
fn distinguishes_invalid_roots_from_invalid_segments() {
    for task_path in [
        "",
        "/",
        "root",
        "backend/auth",
        "/other/task",
        "/ROOT/task",
        "/rooted/task",
        "/root\\task",
    ] {
        assert_eq!(
            validate_canonical_task_path(task_path),
            Err(TaskPathValidationError::InvalidRoot),
            "{task_path:?}",
        );
    }
    for task_path in [
        "/root/",
        "/root//auth",
        "/root/auth/",
        "/root/.",
        "/root/..",
        "/root/a/./b",
        "/root/a/../b",
        "/root/two words",
        "/root/a\tb",
        "/root/a\nb",
        "/root/a\u{00a0}b",
        "/root/a\u{2003}b",
        "/root/a\u{0000}b",
        "/root/a\u{007f}b",
        "/root/a\\b",
    ] {
        assert_eq!(
            validate_canonical_task_path(task_path),
            Err(TaskPathValidationError::InvalidSegment),
            "{task_path:?}",
        );
    }
}
