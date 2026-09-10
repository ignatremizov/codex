//! Shortcut eligibility, separate from the historical picker inventory.

use super::AgentNavigationDirection;
use super::AgentNavigationState;
use codex_app_server_protocol::AgentAliasState;
use codex_protocol::ThreadId;

impl AgentNavigationState {
    pub(crate) fn is_cycle_member(&self, root: ThreadId, thread_id: ThreadId) -> bool {
        if self.get(&thread_id).is_none_or(|entry| entry.is_closed) {
            return false;
        }
        if let Some(alias) = self.alias(thread_id) {
            if alias.state != AgentAliasState::Active {
                return false;
            }
            if self.root_thread_id() == Some(root) {
                return true;
            }
        }
        if thread_id == root {
            return true;
        }
        let mut ancestor = thread_id;
        for _ in 0..self.parent_threads.len() {
            let Some(parent) = self.parent_thread_id(ancestor) else {
                return false;
            };
            if parent == root {
                return true;
            }
            ancestor = parent;
        }
        false
    }

    /// Walk the historical order without removing closed rows from the picker.
    ///
    /// The current row may itself be closed. It remains the traversal anchor, not a candidate.
    /// Availability includes caller-owned attachment/transport state and attempted candidates.
    pub(crate) fn adjacent_available_thread_id(
        &self,
        root: ThreadId,
        current: Option<ThreadId>,
        direction: AgentNavigationDirection,
        is_available: impl Fn(ThreadId) -> bool,
    ) -> Option<ThreadId> {
        self.adjacent_thread_id(current, direction, |id| {
            self.is_cycle_member(root, id) && is_available(id)
        })
    }
}

#[cfg(test)]
#[path = "agent_cycle_order_tests.rs"]
mod tests;
