use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_thread_store::StoredSubAgentCompletionPresentation;
use std::collections::HashMap;

/// Positive receipts belonging only to this ephemeral runtime, never reconstructed from history.
#[derive(Default)]
pub(crate) struct CompletionRuntimeProvenance {
    pub(crate) contexts: HashMap<ResponseItemId, ResponseItemEnvelope>,
    pub(crate) presentations: HashMap<(String, String), StoredSubAgentCompletionPresentation>,
}

/// Canonically acknowledged completion evidence retained across stale compaction requests.
///
/// Pending entries are checkpoint-only until their mailbox lease is consumed. Installed entries
/// remain retained until a successfully published checkpoint proves its request included that
/// exact payload: consuming a lease does not prove that an already-computed replacement saw it.
pub(crate) struct AcknowledgedCompletionContext {
    pub(crate) item: ResponseItemEnvelope,
    pub(crate) pending: bool,
}
