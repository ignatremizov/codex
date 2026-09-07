use super::resolve_task_path;
use pretty_assertions::assert_eq;

#[test]
fn resolves_labels_without_changing_case_or_unicode() {
    for (caller, label, expected) in [
        (None, "backend/auth", "/root/backend/auth"),
        (
            Some("/root/backend"),
            "Review/認証",
            "/root/backend/Review/認証",
        ),
        (Some("/root/backend"), "/root/Frontend", "/root/Frontend"),
        (None, "/root", "/root"),
    ] {
        assert_eq!(
            resolve_task_path(caller, label).ok().as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn rejects_noncanonical_segments_instead_of_normalizing_them() {
    for label in [
        "",
        "/",
        "/other/task",
        "/root/",
        "/root//task",
        ".",
        "..",
        "a/./b",
        "a/../b",
        "two words",
        "a\tb",
        "a\nb",
        "a\u{00a0}b",
        "a\u{0000}b",
        "a\\b",
    ] {
        assert!(
            resolve_task_path(Some("/root/backend"), label).is_err(),
            "{label:?}"
        );
    }
}
