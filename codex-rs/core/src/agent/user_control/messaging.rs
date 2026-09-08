//! User-owned directed and subtree messaging controls.

use super::*;
use crate::agent::control::TargetMessageRouteMode;

impl CodexThread {
    /// Persist a messaging default for this supervisor and its current/future descendants.
    /// The setting survives restart independently of response subscriptions.
    pub async fn set_agent_subtree_messaging(
        &self,
        mode: UserAgentReplyRouteMode,
    ) -> CodexResult<Option<UserAgentReplyRouteMode>> {
        let mode = match mode {
            UserAgentReplyRouteMode::Enabled => TargetMessageRouteMode::Enabled,
            UserAgentReplyRouteMode::Disabled => TargetMessageRouteMode::Disabled,
        };
        self.session
            .services
            .agent_control
            .set_subtree_messaging(self.session.presentation_id(), mode)
            .await
            .map(|previous| {
                previous.map(|mode| match mode {
                    TargetMessageRouteMode::Enabled => UserAgentReplyRouteMode::Enabled,
                    TargetMessageRouteMode::Disabled => UserAgentReplyRouteMode::Disabled,
                })
            })
    }

    /// Enable or disable a sender's attributed reply route within this control graph.
    /// Omitted recipient means the thread issuing the user command.
    pub async fn set_agent_reply_route(
        &self,
        target: &str,
        recipient: Option<&str>,
        mode: UserAgentReplyRouteMode,
    ) -> CodexResult<(ThreadId, ThreadId, Option<UserAgentReplyRouteMode>)> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(self.session.thread_id(), target)
            .await?;
        let recipient_thread_id = match recipient {
            Some(recipient) => {
                agent_control
                    .resolve_controlled_agent_target(self.session.thread_id(), recipient)
                    .await?
            }
            None => source_thread_id,
        };
        if target_thread_id == recipient_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot grant itself a reply route".to_string(),
            ));
        }
        if mode == UserAgentReplyRouteMode::Disabled
            && agent_control
                .is_live_agent_descendant(target_thread_id, recipient_thread_id)
                .await?
        {
            return Err(CodexErr::InvalidRequest(
                "cannot disable supervisor send_input to a descendant; task dispatch remains enabled"
                    .to_string(),
            ));
        }
        if matches!(
            agent_control.get_status(target_thread_id).await,
            AgentStatus::NotFound
        ) {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {target_thread_id} is closed"
            )));
        }
        let replacement = match mode {
            UserAgentReplyRouteMode::Enabled => TargetMessageRouteMode::Enabled,
            UserAgentReplyRouteMode::Disabled => TargetMessageRouteMode::Disabled,
        };
        let replaced = agent_control
            .replace_durable_target_message_route(
                target_thread_id,
                recipient_thread_id,
                replacement,
            )
            .await?;
        let previous = replaced.previous.map(|mode| match mode {
            TargetMessageRouteMode::Enabled => UserAgentReplyRouteMode::Enabled,
            TargetMessageRouteMode::Disabled => UserAgentReplyRouteMode::Disabled,
        });
        Ok((replaced.target_thread_id, recipient_thread_id, previous))
    }
}
