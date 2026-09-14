use super::*;

impl Session {
    /// The canonical delivery is already acknowledged. Queue retirement is a separate,
    /// retryable database boundary; failures cannot undo or republish the response.
    pub(super) async fn acknowledge_mailbox_final_subscription_delivery(
        &self,
        commit: &crate::agent::control::ResponseObservationDeliveryCommit,
    ) {
        let Some(message_id) = commit.mailbox_final_subscription_message_id.as_ref() else {
            return;
        };
        if let Err(error) = self
            .services
            .thread_store
            .acknowledge_mailbox_final_subscription_delivery(
                commit.child.thread_id,
                message_id.clone(),
                commit.turn_id.clone(),
            )
            .await
        {
            tracing::warn!(%error, %message_id, "mailbox final subscription acknowledgement failed");
        }
    }
}
