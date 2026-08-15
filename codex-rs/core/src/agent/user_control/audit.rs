use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::UserAgentControlItem;

use crate::CodexThread;

impl CodexThread {
    /// Persist and publish a user-authored agent control action in the source transcript.
    pub async fn record_user_agent_control(&self, item: UserAgentControlItem) -> CodexResult<()> {
        self.session.record_user_agent_control(item).await
    }
}
