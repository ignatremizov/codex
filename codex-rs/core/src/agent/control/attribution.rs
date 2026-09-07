//! Captures trusted input provenance independently of routing permission.

use super::*;
use crate::context::AgentContextIdentity;
use crate::context::AttributedAgentMessage;
use crate::context::ContextualUserFragment;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;

impl LocalAgentControl {
    /// Capture trusted send-time attribution without changing admission or reply permission.
    pub(crate) async fn attribute_model_input(
        &self,
        sender: SessionPresentationId,
        recipient: ThreadId,
        sender_turn_id: &str,
        input: Vec<UserInput>,
    ) -> CodexResult<AgentControlInput> {
        let state = self.upgrade()?;
        let sender_thread = state.get_thread(sender.thread_id).await?;
        if sender_thread.session.presentation_id() != sender {
            return Err(CodexErr::ThreadNotFound(sender.thread_id));
        }
        let (sender_context, sender_identity) = self.agent_input_identity(sender.thread_id).await?;
        let (_, recipient_identity) = self.agent_input_identity(recipient).await?;
        // The original typed items, including attachment metadata and text-element spans,
        // remain in the durable presentation. Only model-facing text is enveloped.
        let message = render_input_preview(&input);
        let mut content = vec![UserInput::Text {
            text: AttributedAgentMessage::new(sender_context, message).render(),
            text_elements: Vec::new(),
        }];
        content.extend(
            input
                .iter()
                .filter(|item| !matches!(item, UserInput::Text { .. }))
                .cloned(),
        );
        Ok(AgentControlInput::AttributedAgentInput {
            content,
            attribution: Box::new(AgentInputAttribution {
                sender: sender_identity,
                recipient: recipient_identity,
                sender_turn_id: sender_turn_id.to_string(),
            }),
            presentation: input,
        })
    }

    async fn agent_input_identity(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<(AgentContextIdentity, AgentInputIdentity)> {
        let identity = self
            .model_visible_agent_identity_for_version(MultiAgentVersion::V1, thread_id)
            .await?;
        let AgentContextIdentity::V1 {
            agent_ref,
            nickname,
            task_path,
            ..
        } = &identity
        else {
            unreachable!("V1 identity resolution always returns a V1 identity")
        };
        let snapshot = self.get_agent_config_snapshot(thread_id).await;
        let metadata = self.get_agent_metadata(thread_id);
        let audit = AgentInputIdentity {
            thread_id,
            nickname: nickname.clone(),
            agent_ref: agent_ref.map(|agent_ref| agent_ref.to_string()),
            task_path: task_path.clone(),
            role: snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.session_source.get_agent_role())
                .or_else(|| metadata.and_then(|metadata| metadata.agent_role)),
            model: snapshot.as_ref().map(|snapshot| snapshot.model.clone()),
            reasoning_effort: snapshot.and_then(|snapshot| snapshot.reasoning_effort),
        };
        Ok((identity, audit))
    }
}
