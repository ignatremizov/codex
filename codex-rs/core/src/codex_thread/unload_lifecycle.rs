//! Ordinary lifecycle mutations cannot overtake an exact runtime's durable unload owner.

use super::CodexThread;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl CodexThread {
    pub(crate) fn ensure_not_unloading(&self) -> CodexResult<()> {
        if self.session.submission_admission.is_sealed_for_unload() {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {} is sealed for durable unload; retry thread/unload",
                self.session.thread_id()
            )));
        }
        Ok(())
    }
}
