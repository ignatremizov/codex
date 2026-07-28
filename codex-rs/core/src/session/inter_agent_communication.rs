use super::input_queue::CompletionCommunicationCommit;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::ThreadStoreError;
use std::sync::Arc;

pub(crate) enum InterAgentCommunicationRecord {
    Ordinary,
    CompletionRecorded,
    CompletionDeferred,
}

impl Session {
    pub(crate) async fn persist_inter_agent_completion_context_without_turn(
        &self,
        turn_context: Arc<TurnContext>,
        mut communication: InterAgentCommunication,
    ) -> Result<(), ThreadStoreError> {
        let Some(response_item_id) = communication
            .id
            .as_ref()
            .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
            .cloned()
        else {
            return Err(ThreadStoreError::InvalidRequest {
                message: "completion communication requires a reserved response item ID"
                    .to_string(),
            });
        };
        let _permit = self.durable_context_lock.acquire().await.map_err(|err| {
            ThreadStoreError::Internal {
                message: format!("failed to lock inter-agent completion recording: {err}"),
            }
        })?;
        if self
            .persisted_sub_agent_completion_context_item(&response_item_id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        if !self
            .services
            .agent_control
            .is_completion_context_response_item_id_authorized(
                self.presentation_id(),
                &response_item_id,
            )
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "completion communication response item ID is not authorized".to_string(),
            });
        }

        communication.set_turn_id_if_missing(&turn_context.sub_id);
        let response_item = communication.to_model_input_item();
        let mut items = self
            .prepare_conversation_items_for_history(
                turn_context.as_ref(),
                std::slice::from_ref(&response_item),
            )
            .into_owned();
        let Some(ResponseItem::AgentMessage { id, .. }) = items.first_mut() else {
            return Err(ThreadStoreError::Internal {
                message: "completion communication did not produce an agent message".to_string(),
            });
        };
        *id = Some(response_item_id);
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items_and_flush_canonical(&[
                    RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: communication.trigger_turn,
                    },
                    RolloutItem::ResponseItem(items.remove(0)),
                ])
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn record_inter_agent_communication(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        mut communication: InterAgentCommunication,
    ) -> InterAgentCommunicationRecord {
        let completion_commit = self
            .input_queue
            .begin_completion_communication_commit(&communication)
            .await;
        let completion_context_response_item_id = match completion_commit {
            CompletionCommunicationCommit::Ordinary => {
                let Ok(_permit) = self.durable_context_lock.acquire().await else {
                    return InterAgentCommunicationRecord::Ordinary;
                };
                communication.set_turn_id_if_missing(&turn_context.sub_id);
                let response_item = communication.to_model_input_item();
                let items = self
                    .prepare_conversation_items_for_history(
                        turn_context.as_ref(),
                        std::slice::from_ref(&response_item),
                    )
                    .into_owned();
                let response_item = items[0].clone();
                {
                    let mut state = self.state.lock().await;
                    state.current_time_reminder.note_recorded_items(&items);
                    state.record_items(
                        items.iter(),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                self.persist_rollout_items(&[
                    RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: communication.trigger_turn,
                    },
                    RolloutItem::ResponseItem(response_item),
                ])
                .await;
                self.send_raw_response_items(turn_context.as_ref(), &items)
                    .await;
                return InterAgentCommunicationRecord::Ordinary;
            }
            CompletionCommunicationCommit::Started(response_item_id) => response_item_id,
            CompletionCommunicationCommit::AlreadyStarted => {
                return InterAgentCommunicationRecord::CompletionDeferred;
            }
        };
        let sess = Arc::clone(self);
        let completion_context_response_item_id_for_join =
            completion_context_response_item_id.clone();
        let join_result = tokio::spawn(async move {
            let result: Result<(), ThreadStoreError> = async {
                let _permit = sess.durable_context_lock.acquire().await.map_err(|err| {
                    ThreadStoreError::Internal {
                        message: format!(
                            "failed to lock inter-agent communication recording: {err}"
                        ),
                    }
                })?;
                communication.set_turn_id_if_missing(&turn_context.sub_id);
                let trusted_completion_context_response_item_id = communication
                    .id
                    .as_ref()
                    .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
                    .filter(|id| {
                        sess.services
                            .agent_control
                            .is_completion_context_response_item_id_authorized(
                                sess.presentation_id(),
                                id,
                            )
                    })
                    .cloned();
                let response_item = communication.to_model_input_item();
                let mut items = sess
                    .prepare_conversation_items_for_history(
                        turn_context.as_ref(),
                        std::slice::from_ref(&response_item),
                    )
                    .into_owned();
                if let (
                    Some(trusted_completion_context_response_item_id),
                    Some(ResponseItem::AgentMessage { id, .. }),
                ) = (
                    trusted_completion_context_response_item_id.as_ref(),
                    items.first_mut(),
                ) {
                    *id = Some(trusted_completion_context_response_item_id.clone());
                }
                let persisted_completion_item = if let Some(response_item_id) =
                    trusted_completion_context_response_item_id.as_ref()
                {
                    sess.persisted_sub_agent_completion_context_item(response_item_id)
                        .await?
                } else {
                    None
                };
                if let Some(persisted_completion_item) = persisted_completion_item {
                    items[0] = persisted_completion_item;
                } else {
                    let response_item = items[0].clone();
                    sess.try_persist_rollout_items(&[
                        RolloutItem::InterAgentCommunicationMetadata {
                            trigger_turn: communication.trigger_turn,
                        },
                        RolloutItem::ResponseItem(response_item),
                    ])
                    .await?;
                }
                {
                    let mut state = sess.state.lock().await;
                    state.current_time_reminder.note_recorded_items(&items);
                    state.record_items(
                        items.iter(),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                if let Some(response_item_id) = trusted_completion_context_response_item_id.as_ref()
                {
                    let claimed = sess
                        .services
                        .agent_control
                        .claim_completion_context_response_item_id(
                            sess.presentation_id(),
                            response_item_id,
                        );
                    if !claimed {
                        tracing::error!(
                            response_item_id = %response_item_id,
                            "durably recorded completion context lost its authorization"
                        );
                    }
                }
                sess.send_raw_response_items(turn_context.as_ref(), &items)
                    .await;
                sess.input_queue
                    .acknowledge_completion_communication(&completion_context_response_item_id)
                    .await;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => InterAgentCommunicationRecord::CompletionRecorded,
                Err(err) => {
                    tracing::error!("failed to record inter-agent communication: {err:#}");
                    sess.input_queue
                        .retry_completion_communication(&completion_context_response_item_id)
                        .await;
                    InterAgentCommunicationRecord::CompletionDeferred
                }
            }
        })
        .await;
        match join_result {
            Ok(record) => record,
            Err(err) => {
                tracing::error!("inter-agent communication recording task failed: {err}");
                self.input_queue
                    .retry_completion_communication(&completion_context_response_item_id_for_join)
                    .await;
                InterAgentCommunicationRecord::CompletionDeferred
            }
        }
    }
}
