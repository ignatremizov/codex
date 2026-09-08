//! Explicit mailbox consumption at the accepted direct tool-result boundary.
//!
//! Claims reserve membership, canonical history proves delivery, and the live
//! history is only a projection. Neither selection nor a successful handler
//! result grants permission or acknowledges a message.

use super::session::Session;
use super::turn_context::TurnContext;
use crate::agent::control::render_input_preview;
use crate::context::AgentContextIdentity;
use crate::context::AttributedAgentMessage;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ClaimMailboxInputParams;
use codex_thread_store::MailboxDeliveryArtifacts;
use codex_thread_store::MailboxDeliveryEvidence;
use codex_thread_store::MailboxInvocation;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSelection;
use codex_thread_store::MailboxSender;
use codex_thread_store::ReconcileMailboxDeliveryParams;
use codex_thread_store::RejectMailboxInputParams;
use codex_thread_store::ThreadStoreError;
use std::collections::HashSet;
use std::sync::Arc;

/// A canonical selection accompanying an accepted direct result, not a delivery receipt.
#[derive(Clone, Debug)]
pub(crate) struct MailboxConsumption {
    pub(crate) tool_call_id: String,
    pub(crate) selection: MailboxSelection,
}

impl Session {
    /// Commit result first, then structured mail, before allowing another sample.
    ///
    /// The detached task retains both arbitration locks across ambiguous writes,
    /// live insertion and checked acknowledgement even if the caller is cancelled.
    pub(crate) async fn commit_mailbox_consumption(
        self: &Arc<Self>,
        turn: Arc<TurnContext>,
        mut result: ResponseItemEnvelope,
        operation: MailboxConsumption,
    ) -> Result<(), ThreadStoreError> {
        let session = Arc::clone(self);
        let commit = async move {
            let control = &session.services.agent_control;
            let mut warnings = Vec::new();
            let outcome = async {
                let _messaging = control.acquire_messaging_permission_transaction().await;
                let _durable = session
                    .durable_context_lock
                    .acquire()
                    .await
                    .map_err(|error| ThreadStoreError::Internal {
                        message: format!("failed to lock mailbox context recording: {error}"),
                    })?;
                let live =
                    session
                        .live_thread()
                        .ok_or_else(|| ThreadStoreError::InvalidRequest {
                            message: "mailbox consumption requires canonical thread persistence"
                                .to_string(),
                        })?;
                live.flush_canonical().await?;
                let store = &session.services.thread_store;
                let claim_params = ClaimMailboxInputParams {
                    invocation: MailboxInvocation {
                        receiver_thread_id: session.thread_id,
                        turn_id: turn.sub_id.clone(),
                        tool_call_id: operation.tool_call_id.clone(),
                    },
                    selection: operation.selection,
                };
                let recovered = store.recover_mailbox_delivery(claim_params.clone()).await?;
                if recovered.claim.messages.len() != recovered.deliveries.len() {
                    return Err(ThreadStoreError::Internal {
                        message: "mailbox recovery returned incomplete membership".to_string(),
                    });
                }

                // A lost append receipt must reuse the original accepted result too.
                let history = live
                    .load_rollback_history(/*include_archived*/ false)
                    .await?;
                let mut existing_result = None;
                let mut result_index = None;
                for (index, item) in history.items.iter().enumerate() {
                    if let RolloutItem::ResponseItem(envelope) = item
                        && envelope.item.turn_id() == Some(turn.sub_id.as_str())
                        && matches!(&envelope.item, ResponseItem::FunctionCallOutput {
                        call_id: Some(call_id), ..
                    } if call_id == &operation.tool_call_id)
                    {
                        if existing_result
                            .as_ref()
                            .is_some_and(|previous| previous != envelope)
                        {
                            return Err(ThreadStoreError::Conflict {
                                message: "conflicting canonical mailbox tool results".to_string(),
                            });
                        }
                        existing_result = Some(envelope.clone());
                        result_index.get_or_insert(index);
                    }
                }
                let delivery_ids = recovered
                    .claim
                    .messages
                    .iter()
                    .filter(|member| member.message.state == MailboxMessageState::Claimed)
                    .map(|member| {
                        codex_protocol::mailbox_delivery_response_item_id(&member.delivery_id)
                            .ok_or_else(|| ThreadStoreError::Internal {
                                message: "mailbox claim has an invalid delivery identity"
                                    .to_string(),
                            })
                            .map(|id| id.to_string())
                    })
                    .collect::<Result<HashSet<_>, _>>()?;
                if history.items.iter().enumerate().any(|(index, item)| {
                    let belongs_to_delivery = match item {
                        RolloutItem::ResponseItem(item) => item
                            .id()
                            .is_some_and(|id| delivery_ids.contains(id.as_str())),
                        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                            delivery_ids.contains(&event.item.id())
                        }
                        _ => false,
                    };
                    belongs_to_delivery
                        && result_index.is_none_or(|result_index| index < result_index)
                }) {
                    return Err(ThreadStoreError::Conflict {
                        message:
                            "mailbox recovery required: delivery precedes its canonical tool result"
                                .to_string(),
                    });
                }
                if let Some(existing) = existing_result {
                    result = existing;
                } else {
                    Session::stamp_response_item_for_history(&mut result.item, &turn.sub_id);
                    result.item.set_id(Some(ResponseItemId::new("fco")));
                    live.append_items_and_flush_canonical(&[RolloutItem::ResponseItem(
                        result.clone(),
                    )])
                    .await?;
                }

                session.insert_mailbox_context(&turn, result).await;
                let mut evidence = Vec::new();
                for (member, delivery) in recovered.claim.messages.iter().zip(recovered.deliveries)
                {
                    if member.message.id != delivery.message_id {
                        return Err(ThreadStoreError::Internal {
                            message: "mailbox recovery returned mismatched membership".to_string(),
                        });
                    }
                    match member.message.state {
                        MailboxMessageState::Consumed | MailboxMessageState::Rejected => continue,
                        MailboxMessageState::Pending => {
                            return Err(ThreadStoreError::Internal {
                                message: "mailbox recovery returned unclaimed input".to_string(),
                            });
                        }
                        MailboxMessageState::Claimed => {}
                    }
                    let complete = matches!(
                        &delivery.artifacts,
                        MailboxDeliveryArtifacts::Complete { .. }
                    );
                    // Permission governs the first canonical model-context write.
                    // ContextOnly is already admitted input: repair presentation
                    // from its exact envelope without reauthorizing or rejecting it.
                    let context_was_recorded = matches!(
                        &delivery.artifacts,
                        MailboxDeliveryArtifacts::ContextOnly { .. }
                            | MailboxDeliveryArtifacts::Complete { .. }
                    );
                    let denied_sender = if context_was_recorded {
                        None
                    } else {
                        match &member.message.sender {
                            MailboxSender::User => None,
                            MailboxSender::Agent(sender) => {
                                let allowed = control
                                    .mailbox_send_permission_locked(*sender, session.thread_id)
                                    .await
                                    .map_err(|error| ThreadStoreError::Internal {
                                        message: format!(
                                            "failed to revalidate mailbox permission: {error}"
                                        ),
                                    })?;
                                (!allowed).then_some(*sender)
                            }
                        }
                    };
                    if let Some(sender) = denied_sender {
                        let reason =
                            "mailbox send permission was revoked before consumption".to_string();
                        store
                            .reject_mailbox_input(RejectMailboxInputParams {
                                receiver_thread_id: session.thread_id,
                                message_id: member.message.id.clone(),
                                reason: reason.clone(),
                            })
                            .await?;
                        warnings.push((sender, member.message.id.clone(), reason));
                        continue;
                    }
                    let response_id =
                        codex_protocol::mailbox_delivery_response_item_id(&member.delivery_id)
                            .ok_or_else(|| ThreadStoreError::Internal {
                                message: "mailbox claim has an invalid delivery identity"
                                    .to_string(),
                            })?;
                    let (existing_context, existing_completion) = match delivery.artifacts {
                        MailboxDeliveryArtifacts::NotRecorded => (None, None),
                        MailboxDeliveryArtifacts::ContextOnly { prepared_context } => {
                            (Some(prepared_context), None)
                        }
                        MailboxDeliveryArtifacts::PresentationOnly { completion } => {
                            (None, Some(completion))
                        }
                        MailboxDeliveryArtifacts::Complete {
                            prepared_context,
                            completion,
                        } => (Some(prepared_context), Some(completion)),
                    };
                    let mut append = Vec::new();
                    let context = if let Some(context) = existing_context {
                        context
                    } else {
                        let input = match &member.message.payload {
                            MailboxPayload::User { input, .. } => input.clone(),
                            MailboxPayload::Agent { input, attribution } => {
                                let sender = &attribution.sender;
                                let identity = AgentContextIdentity::V1 {
                                    agent_id: sender.thread_id,
                                    agent_ref: sender
                                        .agent_ref
                                        .as_deref()
                                        .and_then(|value| value.parse().ok()),
                                    nickname: sender.nickname.clone(),
                                    task_path: sender.task_path.clone(),
                                };
                                let mut content = vec![UserInput::Text {
                                    text: AttributedAgentMessage::new(
                                        identity,
                                        render_input_preview(input),
                                    )
                                    .render(),
                                    text_elements: Vec::new(),
                                }];
                                content.extend(
                                    input
                                        .iter()
                                        .filter(|item| !matches!(item, UserInput::Text { .. }))
                                        .cloned(),
                                );
                                content
                            }
                        };
                        let mut response = session.response_item_from_user_input(input);
                        response.set_id(Some(response_id.clone()));
                        let (prepared, _) = session.prepare_conversation_items_for_history(
                            &turn,
                            std::slice::from_ref(&response),
                        );
                        let context = ResponseItemEnvelope::new(prepared[0].clone());
                        append.push(RolloutItem::ResponseItem(context.clone()));
                        context
                    };
                    let completion_was_missing = existing_completion.is_none();
                    let completion = if let Some(completion) = existing_completion {
                        completion
                    } else {
                        let item = match &member.message.payload {
                            MailboxPayload::User { input, client_id } => {
                                TurnItem::UserMessage(UserMessageItem {
                                    id: response_id.to_string(),
                                    client_id: client_id.clone(),
                                    content: input.clone(),
                                })
                            }
                            MailboxPayload::Agent { input, attribution } => {
                                let mut item = AgentMessageItem::new(&[]);
                                item.id = response_id.to_string();
                                item.phase = Some(MessagePhase::Commentary);
                                item.attribution = Some(*attribution.clone());
                                item.input = Some(input.clone());
                                TurnItem::AgentMessage(item)
                            }
                        };
                        let completion = ItemCompletedEvent {
                            thread_id: session.thread_id,
                            turn_id: turn.sub_id.clone(),
                            item,
                            started_at_ms: None,
                            completed_at_ms: chrono::Utc::now().timestamp_millis(),
                        };
                        append.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                            completion.clone(),
                        )));
                        completion
                    };
                    if !append.is_empty() {
                        live.append_items_and_flush_canonical(&append).await?;
                    }
                    let member_evidence = MailboxDeliveryEvidence {
                        message_id: member.message.id.clone(),
                        prepared_context: context.clone(),
                    };
                    evidence.push(member_evidence.clone());
                    let context_inserted = session.insert_mailbox_context(&turn, context).await;
                    // Canonical presentation repair is independent of live context
                    // deduplication (resume can restore C while P is still missing).
                    if context_inserted || completion_was_missing {
                        session
                            .send_event_raw_with_persistence(
                                Event {
                                    id: turn.sub_id.clone(),
                                    msg: EventMsg::ItemCompleted(completion),
                                },
                                /*persist*/ false,
                            )
                            .await;
                    }
                    if complete {
                        // Canonical complete proof bypasses current permission.
                        // Finish live projection/publication before terminal ack so
                        // a lost SQL receipt cannot strand an incomplete projection.
                        let complete_evidence = vec![member_evidence];
                        let reconciled = store
                            .reconcile_mailbox_delivery(ReconcileMailboxDeliveryParams {
                                claim: claim_params.clone(),
                                deliveries: complete_evidence.clone(),
                            })
                            .await?;
                        ensure_acknowledged(&reconciled, &complete_evidence)?;
                    }
                }
                let reconciled = store
                    .reconcile_mailbox_delivery(ReconcileMailboxDeliveryParams {
                        claim: claim_params,
                        deliveries: evidence.clone(),
                    })
                    .await?;
                ensure_acknowledged(&reconciled, &evidence)
            };
            let outcome = outcome.await;
            // Warning delivery is presentation-only and best effort, outside
            // both receiver arbitration locks. It cannot undo durable rejection.
            for (sender, message_id, reason) in warnings {
                control
                    .publish_mailbox_rejection(sender, session.thread_id, &message_id, &reason)
                    .await;
            }
            outcome
        };
        tokio::spawn(commit)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("mailbox commit task failed: {error}"),
            })?
    }

    /// Projects a canonical envelope once without preparing or appending it again.
    async fn insert_mailbox_context(
        &self,
        turn: &TurnContext,
        envelope: ResponseItemEnvelope,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            if state
                .history
                .raw_items()
                .any(|item| item.id() == envelope.id())
            {
                return false;
            }
            state
                .current_time_reminder
                .note_recorded_items(std::slice::from_ref(&envelope.item));
            state.history.record_annotated_items(
                std::slice::from_ref(&envelope),
                turn.model_info().truncation_policy.into(),
            );
        }
        self.send_raw_response_items(turn, std::slice::from_ref(&envelope.item))
            .await;
        true
    }
}

fn ensure_acknowledged(
    claim: &codex_thread_store::MailboxClaim,
    evidence: &[MailboxDeliveryEvidence],
) -> Result<(), ThreadStoreError> {
    let consumed = claim
        .messages
        .iter()
        .filter(|member| member.message.state == MailboxMessageState::Consumed)
        .map(|member| member.message.id.as_str())
        .collect::<HashSet<_>>();
    if evidence
        .iter()
        .any(|delivery| !consumed.contains(delivery.message_id.as_str()))
    {
        return Err(ThreadStoreError::Conflict {
            message: "canonical mailbox delivery could not be acknowledged".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
