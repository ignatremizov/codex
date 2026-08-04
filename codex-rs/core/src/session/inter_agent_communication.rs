use super::input_queue::CompletionCommunicationCommit;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_history::RolloutItem;
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
        let (items, _) = self.prepare_conversation_items_for_history(
            turn_context.as_ref(),
            std::slice::from_ref(&response_item),
        );
        let mut items = items.into_owned();
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
                    RolloutItem::ResponseItem(items.remove(0).into()),
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
        let response_delivery_id = communication.id.clone();
        let response_delivery_commit = response_delivery_id
            .as_ref()
            .and_then(|id| self.registered_communication_delivery_commit(id));
        let completion_commit = self
            .input_queue
            .begin_completion_communication_commit(&communication)
            .await;
        let completion_context_response_item_id = match completion_commit {
            CompletionCommunicationCommit::Ordinary => {
                let _response_observation_transaction =
                    if let Some(commit) = response_delivery_commit.as_ref() {
                        Some(
                            self.services
                                .agent_control
                                .acquire_response_observation_transaction(commit.parent)
                                .await,
                        )
                    } else {
                        None
                    };
                let Ok(_permit) = self.durable_context_lock.acquire().await else {
                    return InterAgentCommunicationRecord::Ordinary;
                };
                let persisted_delivery = if response_delivery_commit.is_some() {
                    match response_delivery_id.as_ref() {
                        Some(response_item_id) => {
                            match self
                                .persisted_response_observation_delivery(response_item_id)
                                .await
                            {
                                Ok(delivery) => Some(delivery),
                                Err(err) => {
                                    tracing::warn!(
                                        "failed to verify observed inter-agent response persistence: {err}"
                                    );
                                    self.cancel_communication_delivery(response_item_id);
                                    return InterAgentCommunicationRecord::Ordinary;
                                }
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                };
                let persisted_response_item = persisted_delivery
                    .as_ref()
                    .and_then(|delivery| delivery.response_item.as_ref());
                communication.set_turn_id_if_missing(&turn_context.sub_id);
                let response_item = communication.to_model_input_item();
                let (items, _) = self.prepare_conversation_items_for_history(
                    turn_context.as_ref(),
                    std::slice::from_ref(&response_item),
                );
                let mut items = items.into_owned();
                if let Some(persisted_response_item) = persisted_response_item {
                    items[0] = persisted_response_item.clone();
                }
                let response_item = items[0].clone();
                let mut rollout_items = vec![
                    RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: communication.trigger_turn,
                    },
                    RolloutItem::ResponseItem(response_item.into()),
                ];
                if let Some(response_delivery_commit) = response_delivery_commit.as_ref() {
                    // Recompute the committed snapshot at the mailbox-consumption boundary. The
                    // watcher durably recorded its pending claim before enqueueing this item, but
                    // it deliberately released the observer transaction while the active parent
                    // finished its current tool call.
                    let delivery_committed = persisted_delivery
                        .as_ref()
                        .is_some_and(|delivery| delivery.committed);
                    if !delivery_committed {
                        if persisted_response_item.is_some() {
                            rollout_items.clear();
                        }
                        rollout_items.extend(
                            self.services
                                .agent_control
                                .deferred_response_observation_commit_snapshots(
                                    response_delivery_commit,
                                )
                                .into_iter()
                                .map(RolloutItem::AgentResponseObservation),
                        );
                        let result = match self.live_thread() {
                            Some(live_thread) => {
                                live_thread
                                    .append_items_and_flush_canonical(&rollout_items)
                                    .await
                            }
                            None => Ok(()),
                        };
                        if let Err(err) = result {
                            tracing::warn!(
                                "failed to durably record observed inter-agent response: {err}"
                            );
                            if let Some(response_item_id) = response_delivery_id.as_ref() {
                                self.cancel_communication_delivery(response_item_id);
                            }
                            return InterAgentCommunicationRecord::Ordinary;
                        }
                    }
                    self.services
                        .agent_control
                        .commit_response_observation_delivery(response_delivery_commit);
                } else {
                    self.persist_rollout_items(&rollout_items).await;
                }
                let response_already_recorded = {
                    let mut state = self.state.lock().await;
                    // A canonical append can succeed even when its caller loses the receipt.
                    // Suppress the retry's second model-context/event insertion by stable item ID.
                    let already_recorded = response_delivery_commit.is_some()
                        && response_delivery_id.as_ref().is_some_and(|id| {
                            state
                                .history
                                .raw_items()
                                .iter()
                                .any(|item| item.id() == Some(id))
                        });
                    if !already_recorded {
                        state.current_time_reminder.note_recorded_items(&items);
                        state.record_items(
                            items.iter(),
                            turn_context.model_info.truncation_policy.into(),
                        );
                    }
                    already_recorded
                };
                if !response_already_recorded {
                    self.send_raw_response_items(turn_context.as_ref(), &items)
                        .await;
                }
                if response_delivery_commit.is_some()
                    && let Some(response_item_id) = response_delivery_id.as_ref()
                {
                    self.resolve_communication_delivery(response_item_id);
                }
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
                let _response_observation_transaction =
                    if let Some(commit) = response_delivery_commit.as_ref() {
                        Some(
                            sess.services
                                .agent_control
                                .acquire_response_observation_transaction(commit.parent)
                                .await,
                        )
                    } else {
                        None
                    };
                let _permit = sess.durable_context_lock.acquire().await.map_err(|err| {
                    ThreadStoreError::Internal {
                        message: format!(
                            "failed to lock inter-agent communication recording: {err}"
                        ),
                    }
                })?;
                communication.set_turn_id_if_missing(&turn_context.sub_id);
                let deferred_delivery_authorizes_completion_item =
                    response_delivery_commit.as_ref().is_some_and(|commit| {
                        commit.parent == sess.presentation_id()
                            && communication.id.as_ref() == Some(&commit.response_item_id)
                    });
                let trusted_completion_context_response_item_id = communication
                    .id
                    .as_ref()
                    .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
                    .filter(|id| {
                        deferred_delivery_authorizes_completion_item
                            || sess
                                .services
                                .agent_control
                                .is_completion_context_response_item_id_authorized(
                                    sess.presentation_id(),
                                    id,
                                )
                    })
                    .cloned();
                let response_item = communication.to_model_input_item();
                let (items, _) = sess.prepare_conversation_items_for_history(
                    turn_context.as_ref(),
                    std::slice::from_ref(&response_item),
                );
                let mut items = items.into_owned();
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
                let persisted_delivery = match response_delivery_id.as_ref() {
                    Some(response_item_id) if response_delivery_commit.is_some() => Some(
                        sess.persisted_response_observation_delivery(response_item_id)
                            .await?,
                    ),
                    Some(_) | None => None,
                };
                if let Some(persisted_completion_item) = persisted_completion_item {
                    items[0] = persisted_completion_item;
                    if let Some(response_delivery_commit) = response_delivery_commit.as_ref()
                        && !persisted_delivery
                            .as_ref()
                            .is_some_and(|delivery| delivery.committed)
                        && let Some(live_thread) = sess.live_thread()
                    {
                        let rollout_suffix = sess
                            .services
                            .agent_control
                            .deferred_response_observation_commit_snapshots(
                                response_delivery_commit,
                            )
                            .into_iter()
                            .map(RolloutItem::AgentResponseObservation)
                            .collect::<Vec<_>>();
                        live_thread
                            .append_items_and_flush_canonical(&rollout_suffix)
                            .await?;
                    }
                } else {
                    let response_item = items[0].clone();
                    let mut rollout_items = vec![
                        RolloutItem::InterAgentCommunicationMetadata {
                            trigger_turn: communication.trigger_turn,
                        },
                        RolloutItem::ResponseItem(response_item.into()),
                    ];
                    if let Some(response_delivery_commit) = response_delivery_commit.as_ref() {
                        rollout_items.extend(
                            sess.services
                                .agent_control
                                .deferred_response_observation_commit_snapshots(
                                    response_delivery_commit,
                                )
                                .into_iter()
                                .map(RolloutItem::AgentResponseObservation),
                        );
                        if let Some(live_thread) = sess.live_thread() {
                            live_thread
                                .append_items_and_flush_canonical(&rollout_items)
                                .await?;
                        }
                    } else {
                        sess.try_persist_rollout_items(&rollout_items).await?;
                    }
                }
                if let Some(response_delivery_commit) = response_delivery_commit.as_ref() {
                    sess.services
                        .agent_control
                        .commit_response_observation_delivery(response_delivery_commit);
                }
                let response_already_recorded = {
                    let mut state = sess.state.lock().await;
                    // Completion retries reuse the same reserved item ID, including when the
                    // canonical write and first in-memory insertion completed before shutdown.
                    let already_recorded = response_delivery_commit.is_some()
                        && response_delivery_id.as_ref().is_some_and(|id| {
                            state
                                .history
                                .raw_items()
                                .iter()
                                .any(|item| item.id() == Some(id))
                        });
                    if !already_recorded {
                        state.current_time_reminder.note_recorded_items(&items);
                        state.record_items(
                            items.iter(),
                            turn_context.model_info.truncation_policy.into(),
                        );
                    }
                    already_recorded
                };
                if let Some(response_item_id) = trusted_completion_context_response_item_id.as_ref()
                {
                    let claimed = sess
                        .services
                        .agent_control
                        .claim_completion_context_response_item_id(
                            sess.presentation_id(),
                            response_item_id,
                        );
                    if !claimed && !deferred_delivery_authorizes_completion_item {
                        tracing::error!(
                            response_item_id = %response_item_id,
                            "durably recorded completion context lost its authorization"
                        );
                    }
                }
                if !response_already_recorded {
                    sess.send_raw_response_items(turn_context.as_ref(), &items)
                        .await;
                }
                if response_delivery_commit.is_some()
                    && let Some(response_item_id) = response_delivery_id.as_ref()
                {
                    sess.resolve_communication_delivery(response_item_id);
                }
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
