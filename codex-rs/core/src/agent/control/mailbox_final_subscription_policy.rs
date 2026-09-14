use super::super::*;
use crate::CodexThread;

impl LocalAgentControl {
    /// Called while the permission transaction excludes concurrent acceptance and replacement.
    pub(in crate::agent::control) async fn active_mailbox_subscription_for_policy(
        &self,
        observer: &Arc<CodexThread>,
        child: SessionPresentationId,
    ) -> CodexResult<Option<String>> {
        match observer
            .session
            .services
            .thread_store
            .lookup_active_mailbox_final_subscription(child.thread_id, observer.session.thread_id())
            .await
        {
            Ok(Some(subscription)) => Ok(Some(subscription.message_id)),
            Ok(None) | Err(codex_thread_store::ThreadStoreError::Unsupported { .. }) => Ok(self
                .mailbox_final_subscription_message_id(observer.session.presentation_id(), child)),
            Err(error) => Err(CodexErr::Fatal(format!(
                "mailbox policy lookup failed: {error}"
            ))),
        }
    }

    pub(in crate::agent::control) async fn retire_mailbox_subscription_token(
        &self,
        observer: &Arc<CodexThread>,
        child: SessionPresentationId,
        message_id: &str,
    ) -> CodexResult<()> {
        match observer
            .session
            .services
            .thread_store
            .supersede_mailbox_final_subscription_message(child.thread_id, message_id.to_owned())
            .await
        {
            Ok(()) | Err(codex_thread_store::ThreadStoreError::Unsupported { .. }) => Ok(()),
            Err(error) => {
                observer.session.quarantine_history(format!(
                    "mailbox token retirement outcome unknown: {error}"
                ));
                Err(CodexErr::Fatal(format!(
                    "mailbox token retirement failed: {error}; reload before retry"
                )))
            }
        }
    }
}
