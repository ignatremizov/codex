use super::*;
use codex_protocol::protocol::CollabAgentRef;

impl AgentControl {
    /// Hydrate display identity without loading or adopting the target runtime.
    pub(crate) async fn get_agent_presentation_ref(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<CollabAgentRef> {
        let metadata = self.get_agent_metadata(thread_id).unwrap_or_default();
        let alias = self.current_agent_alias(thread_id).await?;
        Ok(CollabAgentRef {
            thread_id,
            agent_ref: alias.as_ref().map(|alias| alias.agent_ref.to_string()),
            task_path: alias.as_ref().and_then(|alias| alias.task_path.clone()),
            agent_nickname: alias
                .and_then(|alias| alias.nickname)
                .or(metadata.agent_nickname),
            agent_role: metadata.agent_role,
        })
    }
}
