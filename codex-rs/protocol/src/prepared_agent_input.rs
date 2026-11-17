//! Trusted prepared-agent payloads for exact receiving-turn admission.
//!
//! These are neither human user input nor acceptance-only legacy mailbox mail.
//! The controller captures both runtime incarnations before submission. Keeping
//! the original communication separate from its attribution preserves opaque
//! ciphertext and readable audit without making either human authorization.

use crate::AgentInputAttribution;
use crate::ThreadId;
use crate::protocol::InterAgentCommunication;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparedAgentInput {
    pub communication: InterAgentCommunication,
    pub attribution: Box<AgentInputAttribution>,
    #[schemars(with = "String")]
    pub sender_instance_id: uuid::Uuid,
    #[schemars(with = "String")]
    pub recipient_instance_id: uuid::Uuid,
    /// The original canonical tool invocation, distinct from its batch identity.
    pub call_id: String,
}

impl PreparedAgentInput {
    /// Validate shape and receiver identity before loading or admitting work.
    ///
    /// Permission, selected-backend ownership, and live incarnation checks remain
    /// Core's responsibility; deserializing this type grants none of them.
    pub fn validate_receiver(&self, receiver: ThreadId) -> Result<(), &'static str> {
        if self.attribution.recipient.thread_id != receiver
            || self.attribution.sender.thread_id == receiver
        {
            return Err("prepared agent input attribution does not match its endpoints");
        }
        if self.sender_instance_id.is_nil() || self.recipient_instance_id.is_nil() {
            return Err("prepared agent input requires captured runtime incarnations");
        }
        if self.call_id.is_empty()
            || self.attribution.sender_turn_id.is_empty()
            || self
                .attribution
                .batch_id
                .as_deref()
                .is_some_and(str::is_empty)
        {
            return Err("prepared agent input requires original turn and invocation identity");
        }
        let communication = &self.communication;
        if communication.author == communication.recipient
            || communication.id.is_some()
            || communication
                .internal_chat_message_metadata_passthrough
                .is_some()
            || !communication.other_recipients.is_empty()
            || !communication.trigger_turn
            || communication.defer_to_next_turn
        {
            return Err("prepared agent input must be a fresh single-recipient task message");
        }
        let content = communication
            .encrypted_content
            .as_deref()
            .unwrap_or(&communication.content);
        if content.trim().is_empty() {
            return Err("prepared agent input must contain a nonempty payload");
        }
        Ok(())
    }

    /// Readable presentation only. Never substitutes ciphertext for missing audit.
    pub fn readable_content(&self) -> Option<&str> {
        (!self.communication.content.is_empty()).then_some(self.communication.content.as_str())
    }
}

#[cfg(test)]
#[path = "prepared_agent_input_tests.rs"]
mod tests;
