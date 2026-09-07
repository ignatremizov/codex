use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::UserAgentControlItem;

use crate::CodexThread;

impl CodexThread {
    /// Persist and publish a user-authored agent control action in the source transcript.
    pub async fn record_user_agent_control(
        &self,
        mut item: UserAgentControlItem,
    ) -> CodexResult<()> {
        if item.task_path.is_none()
            && let Some(target_thread_id) = item.target_thread_id
        {
            item.task_path = self
                .session
                .services
                .local_agent_runtime
                .user_control(self.session.session_id())?
                .current_agent_alias(target_thread_id)
                .await?
                .and_then(|alias| alias.task_path);
        }
        self.session.record_user_agent_control(item).await
    }
}
