//! Apply resolved child instructions only to disposable inference input.

use super::Session;
use super::turn_context::TurnContext;
use codex_extension_api::SessionIsolation;
use codex_protocol::models::ResponseItem;

impl Session {
    /// Sampling and compaction use the same effective role without rewriting canonical history.
    /// Root sessions and private helpers keep their own instruction contract.
    pub(crate) fn project_child_developer_instructions(
        &self,
        turn_context: &TurnContext,
        input: &mut Vec<ResponseItem>,
    ) {
        if turn_context.session_source.is_non_root_agent()
            && self.isolation != SessionIsolation::Isolated
            && !crate::guardian::is_basic_session_source(&turn_context.session_source)
            && self
                .services
                .thread_extension_data
                .get::<crate::codex_delegate::compaction::CompactionDecoder>()
                .is_none()
        {
            crate::context::project_developer_instructions(
                input,
                turn_context.developer_instructions.as_deref(),
            );
        }
    }
}
