use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_thread_store::StoredSubAgentCompletionPresentation;
use std::collections::HashMap;

/// Positive publication receipts belonging to this exact runtime, never reconstructed from history.
///
/// Context envelopes survive replacement history for both durable and ephemeral sessions. Durable
/// readers still verify canonical store evidence; these volatile receipts never certify a failed
/// writer barrier. Ephemeral presentation receipts preserve their own explicit runtime-only scope.
#[derive(Default)]
pub(crate) struct CompletionPublicationReceipts {
    pub(crate) contexts: HashMap<ResponseItemId, ResponseItemEnvelope>,
    /// Only ephemeral sessions retain presentation lifecycle receipts here.
    pub(crate) presentations: HashMap<(String, String), StoredSubAgentCompletionPresentation>,
}

/// Canonically acknowledged native completion or typed observed-response evidence.
///
/// Pending entries are checkpoint-only until their mailbox lease is consumed. Installed entries
/// remain retained until a successfully published checkpoint proves its request included that
/// exact payload: consuming a lease does not prove that an already-computed replacement saw it.
pub(crate) struct AcknowledgedCompletionContext {
    pub(crate) item: ResponseItemEnvelope,
    pub(crate) pending: bool,
}
