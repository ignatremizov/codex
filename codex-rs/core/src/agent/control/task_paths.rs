//! Root-scoped assignment labels, independent of lifecycle `AgentPath` ancestry.

use codex_agent_graph_store::AgentAliasState;
use codex_protocol::TaskPathValidationError;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::validate_canonical_task_path;

use super::AgentControl;

impl AgentControl {
    /// Resolve an assignment label or directory prefix using the caller's current durable alias.
    pub(crate) async fn resolve_agent_task_path(
        &self,
        caller_thread_id: ThreadId,
        path: &str,
    ) -> CodexResult<String> {
        if path.starts_with('/') {
            return resolve_task_path(/*caller_task_path*/ None, path);
        }
        let alias = self.find_session_agent_alias(caller_thread_id).await?;
        let caller_task_path = alias
            .as_ref()
            .filter(|alias| alias.state != AgentAliasState::Transferred)
            .and_then(|alias| alias.task_path.as_deref());
        resolve_task_path(caller_task_path, path)
    }

    pub(in crate::agent::control) async fn resolve_new_agent_task_path(
        &self,
        caller_thread_id: ThreadId,
        task: &str,
    ) -> CodexResult<String> {
        let task_path = self.resolve_agent_task_path(caller_thread_id, task).await?;
        if task_path == "/root" {
            return Err(CodexErr::InvalidRequest(
                "task path /root is reserved for Main".to_string(),
            ));
        }
        Ok(task_path)
    }
}

fn resolve_task_path(caller_task_path: Option<&str>, path: &str) -> CodexResult<String> {
    let canonical = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", caller_task_path.unwrap_or("/root"))
    };
    match validate_canonical_task_path(&canonical) {
        Ok(()) => Ok(canonical),
        Err(TaskPathValidationError::InvalidRoot) => Err(CodexErr::InvalidRequest(
            "absolute task paths must start with /root/".to_string(),
        )),
        Err(TaskPathValidationError::InvalidSegment) => Err(CodexErr::InvalidRequest(format!(
            "invalid task path {path:?}: segments must be nonempty and cannot contain whitespace, \
             controls, backslashes, or be . or .."
        ))),
    }
}

#[cfg(test)]
#[path = "task_paths_tests.rs"]
mod tests;
