//! Captures trusted input provenance independently of routing permission.

use super::*;
use crate::agent::api::AgentInfo;
use crate::context::AgentContextIdentity;
use crate::context::AttributedAgentMessage;
use crate::context::ContextualUserFragment;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;

impl LocalAgentControl {
    /// Capture trusted send-time attribution without changing admission or reply permission.
    ///
    /// `batch_id` is the sender's canonical shared tool-call identity for an array-target send;
    /// it is absent for singleton sends and older callers.
    pub(crate) async fn attribute_model_input(
        &self,
        sender: SessionPresentationId,
        recipient: ThreadId,
        sender_turn_id: &str,
        batch_id: Option<&str>,
        input: Vec<UserInput>,
    ) -> CodexResult<AgentControlInput> {
        let state = self.runtime.upgrade()?;
        let sender_thread = state.get_thread(sender.thread_id).await?;
        if sender_thread.session.presentation_id() != sender {
            return Err(CodexErr::ThreadNotFound(sender.thread_id));
        }
        let (sender_context, attribution) = self
            .presentation_attribution(sender.thread_id, recipient, sender_turn_id, batch_id)
            .await?;
        // The original typed items, including attachment metadata and text-element spans,
        // remain in the durable presentation. Only model-facing text is enveloped.
        let message = render_input_preview(&input);
        let mut content = vec![UserInput::Text {
            text: AttributedAgentMessage::new(sender_context, message)
                .render()
                .into_text()
                .unwrap_or_default(),
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
            attribution: Box::new(attribution),
            presentation: input,
        })
    }

    /// Snapshot canonical V1 labels without granting input or response delivery permission.
    pub(super) async fn presentation_attribution(
        &self,
        sender: ThreadId,
        recipient: ThreadId,
        sender_turn_id: &str,
        batch_id: Option<&str>,
    ) -> CodexResult<(AgentContextIdentity, AgentInputAttribution)> {
        let (sender_context, sender) = self.agent_input_identity(sender).await?;
        let (_, recipient) = self.agent_input_identity(recipient).await?;
        Ok((
            sender_context,
            AgentInputAttribution {
                sender,
                recipient,
                sender_turn_id: sender_turn_id.to_owned(),
                batch_id: batch_id.map(str::to_owned),
            },
        ))
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
        let snapshot = match self.inspect_agent(thread_id).await {
            Ok(AgentInfo::Loaded { config, .. }) => Some(*config),
            Ok(AgentInfo::Unloaded(_)) | Err(_) => None,
        };
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
