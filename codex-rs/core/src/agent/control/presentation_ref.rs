use super::*;
use codex_protocol::protocol::CollabAgentRef;

impl LocalAgentControl {
    /// Hydrate display identity without loading or adopting the target runtime.
    pub(crate) async fn get_agent_presentation_ref(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<CollabAgentRef> {
        let metadata = self.get_agent_metadata(thread_id).unwrap_or_default();
        let manager = self.upgrade()?;
        let alias = match manager.agent_graph_store() {
            Some(store) if store.supports_agent_aliases() => store
                .find_current_agent_alias_by_thread(thread_id)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "failed to read display identity for agent {thread_id}: {error}"
                    ))
                })?,
            Some(_) | None => None,
        };
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
