//! Task-label lookup and completion projections over the existing alias cache.
//!
//! Relative labels belong to the issuing thread, not necessarily Main. These cached matches
//! are presentation hints; agent/control still resolves and authorizes the authored selector.

use super::AgentNavigationState;
use codex_app_server_protocol::AgentAliasState;
use codex_protocol::ThreadId;

use crate::bottom_pane::AgentPromptTarget;

impl AgentNavigationState {
    pub(crate) fn update_task_path(&mut self, thread_id: ThreadId, task_path: Option<String>) {
        if let Some(alias) = self.aliases.get_mut(&thread_id) {
            alias.task_path = task_path;
        }
    }

    pub(crate) fn thread_id_for_task_path(
        &self,
        issuing_thread: Option<ThreadId>,
        task: &str,
    ) -> Option<ThreadId> {
        let base = issuing_thread
            .and_then(|id| self.alias(id))
            .and_then(|alias| alias.task_path.as_deref())
            .unwrap_or("/root");
        let canonical = if task.starts_with('/') {
            task.to_string()
        } else {
            format!("{base}/{task}")
        };
        self.aliases.iter().find_map(|(thread_id, alias)| {
            (alias.state != AgentAliasState::Transferred
                && (alias.task_path.as_deref() == Some(canonical.as_str())
                    || (canonical == "/root" && alias.agent_ref == 1)))
                .then_some(*thread_id)
        })
    }

    pub(crate) fn task_path_completions(
        &self,
        issuing_thread: Option<ThreadId>,
        targets: &[AgentPromptTarget],
    ) -> Vec<AgentPromptTarget> {
        let base = issuing_thread
            .and_then(|id| self.alias(id))
            .and_then(|alias| alias.task_path.as_deref())
            .unwrap_or("/root");
        let prefix = format!("{base}/");
        let prefix = prefix.as_str();
        targets
            .iter()
            .flat_map(|target| {
                let task = target
                    .thread_id
                    .and_then(|id| self.alias(id))
                    .filter(|alias| alias.state != AgentAliasState::Transferred)
                    .and_then(|alias| {
                        alias
                            .task_path
                            .as_deref()
                            .or_else(|| (alias.agent_ref == 1).then_some("/root"))
                    });
                task.into_iter().flat_map(move |task| {
                    [Some(task), task.strip_prefix(prefix)]
                        .into_iter()
                        .flatten()
                        .map(move |path| AgentPromptTarget {
                            thread_id: target.thread_id,
                            selector: format!("task:{path}"),
                            label: target.label.clone(),
                        })
                })
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "agent_task_paths_tests.rs"]
mod tests;
