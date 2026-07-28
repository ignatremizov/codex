use codex_history::ResponseItemEnvelope;

/// Canonically acknowledged completion evidence retained across stale compaction requests.
///
/// Pending entries are checkpoint-only until their mailbox lease is consumed. Installed entries
/// remain retained until a successfully published checkpoint proves its request included that
/// exact payload: consuming a lease does not prove that an already-computed replacement saw it.
pub(crate) struct AcknowledgedCompletionContext {
    pub(crate) item: ResponseItemEnvelope,
    pub(crate) pending: bool,
}
