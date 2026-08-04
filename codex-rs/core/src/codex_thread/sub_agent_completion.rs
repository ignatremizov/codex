use codex_protocol::ResponseItemId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::sub_agent_completion_item;
use std::sync::Arc;
use std::time::Duration;

use super::CodexThread;
use crate::session::AcceptedCompletionDelivery;
use crate::session::CompletionSubmissionAdmission;
use crate::session::SessionLoopTermination;

impl CodexThread {
    pub(crate) async fn persist_inter_agent_completion_context_without_turn(
        &self,
        mut communication: InterAgentCommunication,
    ) -> bool {
        let session = Arc::clone(&self.session);
        let session_loop_termination = self.io.session_loop_termination.clone();
        let turn_context = session.new_default_turn().await;
        communication.set_turn_id_if_missing(&turn_context.sub_id);
        let mut attempt = 1;
        loop {
            let result = tokio::select! {
                _ = session_loop_termination.clone() => return false,
                result = session.persist_inter_agent_completion_context_without_turn(
                    Arc::clone(&turn_context),
                    communication.clone(),
                ) => result,
            };
            match result {
                Ok(()) => return true,
                Err(codex_thread_store::ThreadStoreError::InvalidRequest { message }) => {
                    tracing::error!("refusing invalid inter-agent completion context: {message}");
                    return false;
                }
                Err(err) => {
                    tracing::warn!(
                        "failed to record inter-agent completion context; retrying while the parent session is active: {err}"
                    );
                }
            }
            if !wait_for_completion_retry(&session_loop_termination, &mut attempt).await {
                return false;
            }
        }
    }

    pub(crate) async fn emit_sub_agent_completion_without_turn(
        &self,
        agent_reference: &str,
        status: &AgentStatus,
    ) {
        self.emit_sub_agent_completion_with_admission(
            agent_reference,
            status,
            CompletionEmissionAdmission::Ordinary,
        )
        .await;
    }

    pub(crate) async fn emit_accepted_sub_agent_completion_without_turn(
        &self,
        agent_reference: &str,
        status: &AgentStatus,
        completion_delivery: AcceptedCompletionDelivery,
    ) {
        self.emit_sub_agent_completion_with_admission(
            agent_reference,
            status,
            CompletionEmissionAdmission::Accepted(completion_delivery),
        )
        .await;
    }

    async fn emit_sub_agent_completion_with_admission(
        &self,
        agent_reference: &str,
        status: &AgentStatus,
        admission: CompletionEmissionAdmission,
    ) {
        let Some(item) = sub_agent_completion_item(agent_reference, status) else {
            return;
        };
        let accepted_completion_delivery = match admission {
            CompletionEmissionAdmission::Ordinary => None,
            CompletionEmissionAdmission::Accepted(completion_delivery) => Some(completion_delivery),
        };
        let session = Arc::clone(&self.session);
        let session_loop_termination = self.io.session_loop_termination.clone();
        let (first_attempt_tx, first_attempt_rx) = tokio::sync::oneshot::channel();
        let history_only_turn_id = uuid::Uuid::now_v7().to_string();
        tokio::spawn(async move {
            let _accepted_completion_delivery = accepted_completion_delivery;
            let item = TurnItem::AgentMessage(item);
            let mut first_attempt_tx = Some(first_attempt_tx);
            let mut attempt = 1;
            loop {
                let result = tokio::select! {
                    _ = session_loop_termination.clone() => {
                        if let Some(first_attempt_tx) = first_attempt_tx.take() {
                            let _ = first_attempt_tx.send(());
                        }
                        return;
                    }
                    result = session.emit_turn_item_completed_without_turn_with_history_id(
                        item.clone(),
                        &history_only_turn_id,
                    ) => result,
                };
                if let Some(first_attempt_tx) = first_attempt_tx.take() {
                    let _ = first_attempt_tx.send(());
                }
                match result {
                    Ok(()) => return,
                    Err(err) => {
                        tracing::warn!(
                            "failed to record subagent completion; retrying while the parent session is active: {err}"
                        );
                    }
                }
                if !wait_for_completion_retry(&session_loop_termination, &mut attempt).await {
                    return;
                }
            }
        });
        let _ = first_attempt_rx.await;
    }

    pub(crate) async fn persist_sub_agent_notification_without_turn(
        &self,
        message: String,
        admission: CompletionSubmissionAdmission,
        response_item_id: ResponseItemId,
        committed_observations: Vec<AgentResponseObservation>,
    ) -> bool {
        let session = Arc::clone(&self.session);
        let result = tokio::select! {
            _ = self.io.session_loop_termination.clone() => return false,
            result = session.record_sub_agent_notification_with_observation_commit(
                message,
                response_item_id,
                admission,
                committed_observations,
            ) => result,
        };
        match result {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    "failed to record observed subagent notification; watcher will retry: {err}"
                );
                false
            }
        }
    }
}

enum CompletionEmissionAdmission {
    Ordinary,
    Accepted(AcceptedCompletionDelivery),
}

async fn wait_for_completion_retry(
    session_loop_termination: &SessionLoopTermination,
    attempt: &mut u64,
) -> bool {
    let delay = crate::util::backoff(*attempt).min(Duration::from_secs(5));
    *attempt = attempt.saturating_add(1);
    tokio::select! {
        _ = session_loop_termination.clone() => false,
        () = tokio::time::sleep(delay) => true,
    }
}
