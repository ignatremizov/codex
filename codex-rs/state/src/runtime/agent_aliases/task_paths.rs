use std::collections::HashSet;

use codex_protocol::SessionId;
use codex_protocol::TaskPathValidationError;
use codex_protocol::ThreadId;
use codex_protocol::validate_canonical_task_path;
use sqlx::Sqlite;

/// Validate already-resolved assignment labels at the durable allocation boundary.
/// Caller-relative resolution belongs to the caller, not the persistence layer.
pub(super) fn validate_task_path(task_path: Option<&str>) -> anyhow::Result<()> {
    let Some(task_path) = task_path else {
        return Ok(());
    };
    if task_path == "/root" {
        anyhow::bail!("task path /root is reserved for Main");
    }
    match validate_canonical_task_path(task_path) {
        Ok(()) => Ok(()),
        Err(TaskPathValidationError::InvalidRoot) => {
            anyhow::bail!("task path {task_path:?} must be canonical and begin with /root/");
        }
        Err(TaskPathValidationError::InvalidSegment) => {
            anyhow::bail!("task path {task_path:?} contains an invalid segment");
        }
    }
}

pub(super) async fn require_available_task_path(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    task_path: Option<&str>,
) -> anyhow::Result<()> {
    validate_task_path(task_path)?;
    let Some(task_path) = task_path else {
        return Ok(());
    };
    let existing = sqlx::query_as::<_, (String, i64, String)>(
        r#"
SELECT alias.thread_id, alias.agent_ref, COALESCE(edge.status, 'open')
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ? AND alias.task_path = ? AND alias.ownership_state = 'current'
        "#,
    )
    .bind(session_id.to_string())
    .bind(task_path)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((thread_id, agent_ref, status)) = existing {
        let advice = if status == "closed" {
            "resume the existing agent"
        } else {
            "use the existing agent"
        };
        anyhow::bail!(
            "task path {task_path:?} is already owned by ref {agent_ref}, thread {thread_id}, \
             status {status}; {advice}, or choose a different explicit task path"
        );
    }
    Ok(())
}

pub(super) async fn resolve_imported_task_paths(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    selected_thread_id: ThreadId,
    requested_task_path: Option<&str>,
    mut imported: Vec<(ThreadId, Option<String>)>,
) -> anyhow::Result<Vec<crate::AgentTaskPathMapping>> {
    let mut occupied = sqlx::query_scalar::<_, String>(
        "SELECT task_path FROM agent_aliases WHERE session_id = ? \
         AND ownership_state = 'current' AND task_path IS NOT NULL",
    )
    .bind(session_id.to_string())
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    imported.sort_by(|left, right| left.1.cmp(&right.1));
    let mut mapping: Vec<crate::AgentTaskPathMapping> = Vec::new();
    let mut imported = imported.into_iter();
    while let Some((thread_id, previous_task_path)) = imported.next() {
        let base = projected_task_path(
            thread_id,
            previous_task_path.as_deref(),
            selected_thread_id,
            requested_task_path,
            &mapping,
        );
        let task_path = if let Some(base) = base {
            let pending = imported
                .as_slice()
                .iter()
                .filter(|(thread_id, path)| {
                    // Descendant assignment prefixes will move below this member's resolved
                    // label; their former labels must not force an unnecessary suffix.
                    (*thread_id == selected_thread_id && requested_task_path.is_some())
                        || !previous_task_path.as_deref().is_some_and(|prefix| {
                            path.as_deref()
                                .and_then(|path| path.strip_prefix(prefix))
                                .is_some_and(|suffix| suffix.starts_with('/'))
                        })
                })
                .filter_map(|(thread_id, path)| {
                    projected_task_path(
                        *thread_id,
                        path.as_deref(),
                        selected_thread_id,
                        requested_task_path,
                        &mapping,
                    )
                })
                .collect::<HashSet<_>>();
            let mut task_path = base.clone();
            let mut suffix = 2_u64;
            while occupied.contains(&task_path) || pending.contains(&task_path) {
                task_path = format!("{base}-{suffix}");
                suffix = suffix
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("imported task path suffix overflowed"))?;
            }
            occupied.insert(task_path.clone());
            Some(task_path)
        } else {
            None
        };
        if task_path != previous_task_path {
            mapping.push(crate::AgentTaskPathMapping {
                thread_id,
                previous_task_path,
                task_path,
            });
        }
    }
    Ok(mapping)
}

fn projected_task_path(
    thread_id: ThreadId,
    previous_task_path: Option<&str>,
    selected_thread_id: ThreadId,
    requested_task_path: Option<&str>,
    mapping: &[crate::AgentTaskPathMapping],
) -> Option<String> {
    if thread_id == selected_thread_id && requested_task_path.is_some() {
        return requested_task_path.map(ToString::to_string);
    }
    if previous_task_path == Some("/root") {
        // /root belongs exclusively to destination Main. Without an explicit assignment,
        // a foreign Main becomes unlabeled, never a synthetically named task.
        return None;
    }
    let previous_task_path = previous_task_path?;
    let remapped = mapping.iter().rev().find_map(|ancestor| {
        let old_prefix = ancestor.previous_task_path.as_deref()?;
        let new_prefix = ancestor.task_path.as_deref()?;
        let suffix = previous_task_path
            .strip_prefix(old_prefix)
            .filter(|suffix| suffix.starts_with('/'))?;
        Some(format!("{new_prefix}{suffix}"))
    });
    // Prefix remapping follows task labels, never the lifecycle parent edges.
    Some(remapped.unwrap_or_else(|| previous_task_path.to_string()))
}

#[cfg(test)]
#[path = "task_paths_tests.rs"]
mod tests;
