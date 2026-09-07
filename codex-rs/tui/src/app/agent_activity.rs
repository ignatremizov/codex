//! Derives the ambient agent count from existing live navigation state, without discovery.

use super::AgentNavigationState;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;

impl AgentNavigationState {
    /// Generic status describes liveness, not which exact turn consumes an observation.
    /// Only turn lifecycle events may bind policies or consume a reserved prompt source.
    pub(crate) fn update_visual_status(&mut self, thread_id: ThreadId, status: &ThreadStatus) {
        let Some(entry) = self.threads.get_mut(&thread_id) else {
            return;
        };
        let running = matches!(status, ThreadStatus::Active { .. }) && !entry.is_closed;
        entry.is_running = running;
        if running {
            self.stopped_threads.remove(&thread_id);
        } else {
            self.stopped_threads.insert(thread_id);
        }
    }

    pub(crate) fn running_agent_count(
        &self,
        root: Option<ThreadId>,
        current: Option<ThreadId>,
    ) -> usize {
        let Some(root) = root else {
            return 0;
        };
        let aliases_match_root = self.root_thread_id() == Some(root);
        self.threads
            .iter()
            .filter(|(id, entry)| {
                if **id == root || Some(**id) == current || !entry.is_running || entry.is_closed {
                    return false;
                }
                if let Some(alias) = self.aliases.get(*id) {
                    if alias.state != AgentAliasState::Active {
                        return false;
                    }
                    if aliases_match_root {
                        return true;
                    }
                }
                let mut ancestor = **id;
                // Bound traversal by the existing edge count, including malformed cycles.
                for _ in 0..self.parent_threads.len() {
                    let Some(parent) = self.parent_threads.get(&ancestor) else {
                        return false;
                    };
                    if *parent == root {
                        return true;
                    }
                    ancestor = *parent;
                }
                false
            })
            .count()
    }
}

#[cfg(test)]
#[path = "agent_activity_tests.rs"]
mod tests;
