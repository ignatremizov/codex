use crate::realtime_conversation::handle_audio as handle_realtime_conversation_audio;
use crate::realtime_conversation::handle_close as handle_realtime_conversation_close;
use crate::realtime_conversation::handle_speech as handle_realtime_conversation_speech;
use crate::realtime_conversation::handle_start as handle_realtime_conversation_start;
use crate::realtime_conversation::handle_text as handle_realtime_conversation_text;
use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::protocol::Submission;
use tracing::Instrument;
use tracing::debug_span;
use tracing::info_span;

use crate::session::TurnInput;
use crate::session::inter_agent_communication::InterAgentCommunicationRecord;
use crate::session::rollout_reconstruction::RolloutReconstructionRepairPersistence;
use crate::session::session::Session;
use crate::session::thread_settings;
use crate::session::turn_input;
use crate::thread_rollout_truncation::instruction_positions_in_rollout;

use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::GuardianApprovedAction;
use crate::context::NodeReplReviewEvidence;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_app_server_protocol::materialized_rollback_start;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationListVoicesResponseEvent;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_thread_store::PersistContext;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_thread_store::ThreadStoreError;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
}

pub async fn clean_background_terminals(sess: &Arc<Session>) {
    sess.close_unified_exec_processes().await;
}

pub async fn realtime_conversation_list_voices(sess: &Session, sub_id: String) {
    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::RealtimeConversationListVoicesResponse(
            RealtimeConversationListVoicesResponseEvent {
                voices: RealtimeVoicesList::builtin(),
            },
        ),
    })
    .await;
}

pub async fn user_input_or_turn(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    client_user_message_id: Option<String>,
    parent_turn_id: Option<String>,
    root_turn_id: Option<String>,
) {
    let request = match op {
        Op::UserInput {
            items,
            final_output_json_schema,
            responsesapi_client_metadata,
            additional_context,
            thread_settings,
        } => codex_protocol::turn_input::TurnInputRequest::new(
            codex_protocol::turn_input::TurnInput::UserInput {
                content: items,
                client_id: client_user_message_id,
            },
        )
        .with_thread_settings(thread_settings)
        .with_additional_context(additional_context)
        .with_responses_metadata(responsesapi_client_metadata)
        .on_start(codex_protocol::turn_input::TurnStartOptions {
            final_output_json_schema,
            parent_turn_id,
            root_turn_id,
            ..Default::default()
        }),
        Op::AgentInput {
            items,
            presentation,
        } => codex_protocol::turn_input::TurnInputRequest::new(
            codex_protocol::turn_input::TurnInput::AgentInput {
                content: items,
                presentation,
            },
        )
        .on_start(codex_protocol::turn_input::TurnStartOptions {
            parent_turn_id,
            root_turn_id,
            ..Default::default()
        }),
        _ => unreachable!(),
    };
    let result = turn_input::handle(
        sess,
        request,
        codex_protocol::turn_input::TurnInputMode::StartOrSteer,
        sub_id.clone(),
    )
    .await;
    let error = match result {
        Ok(
            codex_protocol::turn_input::TurnInputSubmission::Started { .. }
            | codex_protocol::turn_input::TurnInputSubmission::Steered { .. },
        ) => None,
        Ok(codex_protocol::turn_input::TurnInputSubmission::NotSubmitted { reason }) => Some(
            CodexErr::InvalidRequest(format!("turn input was not submitted: {reason:?}")),
        ),
        Err(error) => Some(error),
    };
    if let Some(error) = error {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(error.to_error_event(/*message_prefix*/ None)),
        })
        .await;
    }
}

/// Queues an inter-agent message, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
    start_options: codex_protocol::turn_input::TurnStartOptions,
) {
    let trigger_turn = communication.trigger_turn;
    let defer_to_next_turn = communication.defer_to_next_turn;
    if defer_to_next_turn {
        sess.input_queue
            .queue_turn_inputs_for_next_turn(vec![TurnInput::InterAgentCommunication(
                communication,
            )])
            .await;
        crate::agent_communication::emit_agent_communication_receive(&sub_id);
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
        return;
    }
    sess.input_queue
        .enqueue_mailbox_communication(communication, start_options)
        .await;
    crate::agent_communication::emit_agent_communication_receive(&sub_id);
    if trigger_turn || sess.has_outstanding_durable_sleep() {
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
    }
}

pub async fn run_user_shell_command(
    sess: &Arc<Session>,
    sub_id: String,
    command: String,
    timeout_ms: Option<u64>,
) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                timeout_ms,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;
    sess.spawn_task(
        turn_context,
        Vec::new(),
        UserShellCommandTask::new(command, timeout_ms),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::approvals::ElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
) {
    let action = match decision {
        codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    let content = match action {
        // Preserve the legacy fallback for clients that only send an action.
        ElicitationAction::Accept => Some(content.unwrap_or_else(|| serde_json::json!({}))),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
        _ => None,
    };
    let response = ElicitationResponse {
        action,
        content,
        meta,
    };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name, request_id, response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
    }
}

/// Propagate a user's exec approval decision to the session.
/// Also optionally applies an execpolicy amendment.
pub async fn exec_approval(
    sess: &Arc<Session>,
    approval_id: String,
    turn_id: Option<String>,
    decision: ReviewDecision,
) {
    let event_turn_id = turn_id.unwrap_or_else(|| approval_id.clone());
    let Some(tx_approve) = sess.take_pending_approval(&approval_id).await else {
        return;
    };
    if let ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment,
    } = &decision
        && let Err(err) = sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
    {
        let message = format!("Failed to apply execpolicy amendment: {err}");
        tracing::warn!("{message}");
        let warning = EventMsg::Warning(WarningEvent { message });
        sess.send_event_raw(Event {
            id: event_turn_id.clone(),
            msg: warning,
        })
        .await;
    }
    match decision {
        ReviewDecision::Abort => {
            tx_approve.send(ReviewDecision::Abort).ok();
            sess.interrupt_task().await;
        }
        other => {
            tx_approve.send(other).ok();
        }
    }
}

pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => {
            sess.notify_approval(&id, other).await;
        }
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response(&id, response).await;
}

pub async fn request_permissions_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestPermissionsResponse,
) {
    sess.notify_request_permissions_response(&id, response)
        .await;
}

pub async fn dynamic_tool_response(sess: &Arc<Session>, id: String, response: DynamicToolResponse) {
    sess.notify_dynamic_tool_response(&id, response).await;
}

pub fn refresh_mcp_servers(sess: &Session) {
    sess.services.mcp_runtime.reconnect_on_next_refresh();
    sess.request_mcp_runtime_refresh();
}

pub async fn queue_mcp_server_use_context(sess: &Session, server_name: String) {
    if sess
        .current_mcp_inventory_was_direct_at_session_start(server_name.as_str())
        .await
    {
        return;
    }
    let should_render_now = sess.reference_context_item().await.is_some();
    let text = if should_render_now {
        Some(
            sess.render_mcp_server_use_context_text(server_name.as_str())
                .await,
        )
    } else {
        None
    };
    if let Some(text) = text.as_deref()
        && sess
            .latest_mcp_server_use_context_text(server_name.as_str())
            .await
            .as_deref()
            == Some(text)
    {
        return;
    }
    if text.is_none()
        && sess
            .has_queued_mcp_server_use_context(server_name.as_str())
            .await
    {
        return;
    }
    // Cache invariant: `/mcp use` is a forward-only prompt-context injection. It must append at
    // the user-invoked point in history and must not rewrite earlier history or promote MCP tools
    // into the top-level tool contract, because either would invalidate the cached session prefix.
    if let Some(text) = text {
        let response_item = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let has_active_turn = sess.active_turn.lock().await.is_some();
        if has_active_turn
            && sess
                .inject_response_items(vec![response_item.clone()])
                .await
                .is_ok()
        {
            return;
        }

        let turn_context = sess.new_default_turn().await;
        sess.record_conversation_items_silently(
            turn_context.as_ref(),
            std::slice::from_ref(&response_item),
        )
        .await;
    } else {
        sess.queue_mcp_server_use_context_for_first_turn(server_name)
            .await;
    }
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    sess.reload_user_config_layer().await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;

    sess.spawn_task(turn_context, Vec::new(), CompactTask).await;
}

enum ThreadRollbackTarget {
    InstructionTurns(u32),
    MaterializedTurns {
        num_turns: u32,
        expected_start_turn_id: Option<String>,
        expected_turn_count: Option<u32>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadRollbackDisposition {
    Continue,
    ReloadRequired,
}

#[derive(Clone, Copy)]
enum ThreadRollbackErrorDelivery {
    Persist,
    Ephemeral,
}

async fn send_thread_rollback_error(
    sess: &Session,
    sub_id: String,
    message: String,
    delivery: ThreadRollbackErrorDelivery,
) {
    sess.submission_admission.rollback_ready_to_publish();
    send_thread_rollback_error_with_info(
        sess,
        sub_id,
        message,
        CodexErrorInfo::ThreadRollbackFailed,
        delivery,
    )
    .await;
    sess.submission_admission.rollback_completed();
}

async fn send_thread_rollback_error_with_info(
    sess: &Session,
    sub_id: String,
    message: String,
    codex_error_info: CodexErrorInfo,
    delivery: ThreadRollbackErrorDelivery,
) {
    let event = Event {
        id: sub_id,
        msg: EventMsg::Error(ErrorEvent {
            misalignment: None,
            message,
            codex_error_info: Some(codex_error_info),
        }),
    };
    match delivery {
        ThreadRollbackErrorDelivery::Persist => sess.send_event_raw(event).await,
        ThreadRollbackErrorDelivery::Ephemeral => sess.deliver_event_raw(event).await,
    }
}

#[cfg(test)]
pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
    let _disposition = thread_rollback_target(
        sess,
        sub_id,
        ThreadRollbackTarget::InstructionTurns(num_turns),
    )
    .await;
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "the active-turn reservation must cover the complete rollback transaction"
)]
async fn thread_rollback_target(
    sess: &Arc<Session>,
    sub_id: String,
    target: ThreadRollbackTarget,
) -> ThreadRollbackDisposition {
    let num_turns = match &target {
        ThreadRollbackTarget::InstructionTurns(num_turns)
        | ThreadRollbackTarget::MaterializedTurns { num_turns, .. } => *num_turns,
    };
    if num_turns == 0 {
        send_thread_rollback_error(
            sess,
            sub_id,
            "num_turns must be >= 1".to_string(),
            ThreadRollbackErrorDelivery::Persist,
        )
        .await;
        return ThreadRollbackDisposition::Continue;
    }

    let _response_observation_transaction = sess
        .services
        .agent_control
        .acquire_response_observation_transaction(sess.presentation_id())
        .await;
    let Ok(_durable_context_permit) = sess.acquire_durable_context_permit().await else {
        send_thread_rollback_error(
            sess,
            sub_id,
            "failed to acquire thread history for rollback".to_string(),
            ThreadRollbackErrorDelivery::Persist,
        )
        .await;
        return ThreadRollbackDisposition::Continue;
    };
    // Keep the active-turn mutex for the whole transaction. Turn startup uses the same mutex, so
    // this is both the idle check and the reservation that prevents queued work from starting
    // between replay and marker persistence.
    let active_turn_guard = sess.active_turn.lock().await;
    if active_turn_guard.is_some() {
        send_thread_rollback_error(
            sess,
            sub_id,
            "Cannot rollback while a turn is in progress.".to_string(),
            ThreadRollbackErrorDelivery::Persist,
        )
        .await;
        return ThreadRollbackDisposition::Continue;
    }

    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;
    let live_thread = match sess.live_thread_for_persistence("rollback thread") {
        Ok(live_thread) => live_thread,
        Err(_) => {
            send_thread_rollback_error(
                sess,
                turn_context.sub_id.clone(),
                "thread rollback requires persisted thread history".to_string(),
                ThreadRollbackErrorDelivery::Persist,
            )
            .await;
            return ThreadRollbackDisposition::Continue;
        }
    };
    if let Err(err) = live_thread.flush_canonical().await {
        send_thread_rollback_error(
            sess,
            turn_context.sub_id.clone(),
            format!("failed to flush thread persistence for rollback replay: {err}"),
            ThreadRollbackErrorDelivery::Persist,
        )
        .await;
        return ThreadRollbackDisposition::Continue;
    }

    let stored_history = match live_thread
        .load_rollback_history(/*include_archived*/ false)
        .await
    {
        Ok(history) => history,
        Err(err) => {
            send_thread_rollback_error(
                sess,
                turn_context.sub_id.clone(),
                format!("failed to load thread history for rollback replay: {err}"),
                ThreadRollbackErrorDelivery::Persist,
            )
            .await;
            return ThreadRollbackDisposition::Continue;
        }
    };

    let instruction_positions = instruction_positions_in_rollout(&stored_history.items);
    let (num_turns, materialized_turns, rollback_start_index) = match target {
        ThreadRollbackTarget::InstructionTurns(num_turns) => {
            let rollback_boundary_index = instruction_positions
                .len()
                .saturating_sub(usize::try_from(num_turns).unwrap_or(usize::MAX));
            let rollback_start_index = instruction_positions
                .get(rollback_boundary_index)
                .copied()
                .map(|instruction_position| {
                    let segment_start = stored_history.items[..=instruction_position]
                        .iter()
                        .rposition(|item| {
                            matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_)))
                        })
                        .filter(|segment_start| {
                            !stored_history.items[*segment_start..instruction_position]
                                .iter()
                                .any(|item| {
                                    matches!(
                                        item,
                                        RolloutItem::EventMsg(
                                            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                                        )
                                    )
                                })
                        });
                    segment_start
                        .filter(|segment_start| {
                            !instruction_positions[..rollback_boundary_index]
                                .iter()
                                .any(|position| *position >= *segment_start)
                        })
                        .unwrap_or(instruction_position)
                });
            (num_turns, None, rollback_start_index)
        }
        ThreadRollbackTarget::MaterializedTurns {
            num_turns: materialized_turns,
            expected_start_turn_id,
            expected_turn_count,
        } => {
            let rollback_start =
                materialized_rollback_start(&stored_history.items, materialized_turns);
            if let Some(expected_turn_count) = expected_turn_count
                && rollback_start.as_ref().is_none_or(|start| {
                    u32::try_from(start.turn_count).unwrap_or(u32::MAX) != expected_turn_count
                })
            {
                send_thread_rollback_error(
                    sess,
                    turn_context.sub_id.clone(),
                    "thread history changed after selecting the prompt; rollback was not applied"
                        .to_string(),
                    ThreadRollbackErrorDelivery::Ephemeral,
                )
                .await;
                return ThreadRollbackDisposition::Continue;
            }
            if let Some(expected_start_turn_id) = expected_start_turn_id
                && rollback_start
                    .as_ref()
                    .is_none_or(|start| start.turn_id != expected_start_turn_id)
            {
                send_thread_rollback_error(
                    sess,
                    turn_context.sub_id.clone(),
                    "selected prompt no longer identifies the rollback boundary; rollback was not applied"
                        .to_string(),
                    ThreadRollbackErrorDelivery::Ephemeral,
                )
                .await;
                return ThreadRollbackDisposition::Continue;
            }
            let rollback_start_index =
                rollback_start.map(|rollback_start| rollback_start.rollout_index);
            let num_turns = rollback_start_index.map_or(0, |rollback_start_index| {
                instruction_positions
                    .iter()
                    .filter(|position| **position >= rollback_start_index)
                    .count()
            });
            (
                u32::try_from(num_turns).unwrap_or(u32::MAX),
                Some(materialized_turns),
                rollback_start_index,
            )
        }
    };
    let rollback_event = ThreadRolledBackEvent {
        num_turns,
        materialized_turns,
        rollback_start_index: rollback_start_index
            .map(|index| u64::try_from(index).unwrap_or(u64::MAX)),
    };
    let rollback_msg = EventMsg::ThreadRolledBack(rollback_event.clone());
    let mut replay_items = stored_history.items;
    replay_items.push(RolloutItem::EventMsg(rollback_msg.clone()));
    let prepared_reconstruction = sess
        .prepare_rollout_reconstruction(turn_context.as_ref(), replay_items.as_slice())
        .await;
    let required_rollback_repair = prepared_reconstruction.repair.as_ref().filter(|repair| {
        matches!(
            repair.persistence,
            RolloutReconstructionRepairPersistence::Required
        )
    });
    let required_rollback_repair =
        required_rollback_repair.map(|repair| (repair.items.clone(), repair.sanitization));
    let rollback_marker_index = replay_items.len().saturating_sub(1);
    let marker_append_result = live_thread
        .append_items_and_flush_canonical(&[RolloutItem::EventMsg(rollback_msg.clone())])
        .await;
    if let Err(err) = marker_append_result {
        match live_thread
            .load_rollback_history(/*include_archived*/ false)
            .await
        {
            Ok(history)
                if history
                    .items
                    .get(rollback_marker_index)
                    .is_some_and(|item| {
                    matches!(
                        item,
                        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(persisted))
                            if persisted.num_turns == rollback_event.num_turns
                                && persisted.materialized_turns == rollback_event.materialized_turns
                                && persisted.rollback_start_index
                                    == rollback_event.rollback_start_index
                    )
                }) =>
            {
                // Read visibility cannot upgrade a failed durability barrier into a commit
                // acknowledgement. Quarantine and rebuild from whatever survives a cold resume.
                sess.submission_admission.rollback_requires_reload();
                send_thread_rollback_error_with_info(
                    sess,
                    turn_context.sub_id.clone(),
                    format!(
                        "rollback marker was readable after its durability barrier failed; refresh the thread before retrying: {err}"
                    ),
                    CodexErrorInfo::ThreadRollbackCommitUnknown,
                    ThreadRollbackErrorDelivery::Ephemeral,
                )
                .await;
                return ThreadRollbackDisposition::ReloadRequired;
            }
            Ok(_) => {}
            Err(verification_err) => {
                sess.submission_admission.rollback_requires_reload();
                send_thread_rollback_error_with_info(
                    sess,
                    turn_context.sub_id.clone(),
                    format!(
                        "rollback marker persistence outcome could not be verified after an append error; refresh the thread before retrying: append error: {err}; verification error: {verification_err}"
                    ),
                    CodexErrorInfo::ThreadRollbackCommitUnknown,
                    ThreadRollbackErrorDelivery::Ephemeral,
                )
                .await;
                return ThreadRollbackDisposition::ReloadRequired;
            }
        }
        send_thread_rollback_error(
            sess,
            turn_context.sub_id.clone(),
            format!("failed to persist rollback marker before rolling back thread: {err}"),
            ThreadRollbackErrorDelivery::Ephemeral,
        )
        .await;
        return ThreadRollbackDisposition::Continue;
    }
    if let Some((repair_items, sanitization)) = required_rollback_repair.as_ref() {
        let repair_append_result = live_thread
            .append_items_and_flush_canonical(repair_items.as_slice())
            .await;
        let repair_is_durable = match &repair_append_result {
            Ok(()) => true,
            Err(_) => {
                let repair_start = rollback_marker_index.saturating_add(1);
                let repair_end = repair_start.saturating_add(repair_items.len());
                live_thread
                    .load_rollback_history(/*include_archived*/ false)
                    .await
                    .ok()
                    .is_some_and(|history| {
                        history
                            .items
                            .get(repair_start..repair_end)
                            .is_some_and(|persisted_items| {
                                persisted_items.len() == repair_items.len()
                                    && persisted_items.iter().zip(repair_items).all(
                                        |(persisted_item, repair_item)| {
                                            serde_json::to_vec(persisted_item)
                                                .ok()
                                                .zip(serde_json::to_vec(repair_item).ok())
                                                .is_some_and(|(persisted_item, repair_item)| {
                                                    persisted_item == repair_item
                                                })
                                        },
                                    )
                            })
                    })
            }
        };
        if repair_is_durable {
            info!(
                omitted_image_count = sanitization.omitted_image_count,
                omitted_inline_media_bytes = sanitization.omitted_inline_media_bytes,
                "persisted required compacted-media repair after rollback marker"
            );
        } else if let Err(err) = repair_append_result {
            // The rollback marker is committed, so it cannot be retried. Cold replay can
            // regenerate the representation repair, but live state must not acknowledge a
            // rollback whose required durable representation is still missing.
            sess.submission_admission.rollback_requires_reload();
            send_thread_rollback_error_with_info(
                sess,
                turn_context.sub_id.clone(),
                format!(
                    "rollback committed, but its required compacted-media repair failed to persist; refresh the thread before continuing: {err}"
                ),
                CodexErrorInfo::ThreadRollbackCommitUnknown,
                ThreadRollbackErrorDelivery::Ephemeral,
            )
            .await;
            return ThreadRollbackDisposition::ReloadRequired;
        }
    }
    let applied_reconstruction = sess
        .install_rollout_reconstruction(turn_context.as_ref(), prepared_reconstruction)
        .await;
    sess.services
        .thread_extension_data
        .remove::<NodeReplReviewEvidence>();
    sess.guardian_review_session.invalidate().await;
    sess.services
        .agent_control
        .rollout_budget()
        .rearm_reminder(sess.thread_id());
    sess.recompute_token_usage(turn_context.as_ref()).await;

    if required_rollback_repair.is_none()
        && let Some(repair) = applied_reconstruction.repair.as_ref()
        && let Err(err) = sess.persist_reconstruction_repair_with_policy(repair).await
    {
        warn!(%err, "failed to persist compacted-media repair after rollback");
    }
    let response_observations = sess
        .services
        .agent_control
        .response_observation_snapshots_for_parent(sess.presentation_id());
    if !sess
        .persist_agent_response_observations_locked(&response_observations)
        .await
    {
        sess.submission_admission.rollback_requires_reload();
        send_thread_rollback_error_with_info(
            sess,
            turn_context.sub_id.clone(),
            "rollback committed, but response observation state could not be re-persisted; refresh the thread before continuing"
                .to_string(),
            CodexErrorInfo::ThreadRollbackCommitUnknown,
            ThreadRollbackErrorDelivery::Ephemeral,
        )
        .await;
        return ThreadRollbackDisposition::ReloadRequired;
    }

    // Admit ordinary follow-up submissions while keeping completion delivery behind the rollback
    // event. The submission loop and active-turn reservation still serialize actual turn startup.
    sess.submission_admission.rollback_ready_to_publish();
    sess.deliver_event_raw(Event {
        id: turn_context.sub_id.clone(),
        msg: rollback_msg,
    })
    .await;
    sess.submission_admission.rollback_completed();
    drop(active_turn_guard);
    ThreadRollbackDisposition::Continue
}

pub(super) async fn persist_thread_memory_mode_update(
    sess: &Arc<Session>,
    mode: ThreadMemoryMode,
) -> anyhow::Result<()> {
    let live_thread = sess.live_thread_for_persistence("update thread memory mode")?;
    live_thread.persist(PersistContext::Standard).await?;
    live_thread.flush().await?;
    live_thread
        .update_memory_mode(mode, /*include_archived*/ false)
        .await?;
    live_thread.flush().await?;
    Ok(())
}

/// Persists thread-level memory mode metadata for the active session.
///
/// This does not involve the model and only affects whether the thread is
/// eligible for future memory generation.
pub async fn set_thread_memory_mode(sess: &Arc<Session>, sub_id: String, mode: ThreadMemoryMode) {
    if let Err(err) = persist_thread_memory_mode_update(sess, mode).await {
        warn!("Failed to persist thread memory mode update to rollout: {err}");
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

pub(super) async fn shutdown_session_runtime(sess: &Arc<Session>) {
    if let Some(startup_prewarm) = sess.take_session_startup_prewarm().await {
        startup_prewarm.abort().await;
    }
    let _ = sess.conversation.shutdown().await;
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let shell_snapshot_prewarm = sess.state.lock().await.shell_snapshot_prewarm.take();
    if let Some(shell_snapshot_prewarm) = shell_snapshot_prewarm {
        shell_snapshot_prewarm.abort();
        let _ = shell_snapshot_prewarm.await;
    }
    sess.hooks().shutdown().await;
    sess.async_hook_results.close();
    while sess.async_hook_results.try_recv().is_ok() {}
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    if let Err(err) = sess.services.code_mode_service.shutdown().await {
        warn!("failed to shutdown code mode session: {err}");
    }
    sess.stop_mcp_prewarm_worker().await;
    {
        let _refresh = sess.mcp_refresh.acquire().await;
        sess.mcp_refresh.close();
        sess.services.mcp_runtime.shutdown().await;
    }
    sess.guardian_review_session.shutdown().await;

    crate::hook_runtime::run_session_end_hooks(sess).await;
}

pub(super) async fn emit_thread_stop_lifecycle(sess: &Session) {
    for contributor in sess.services.extensions.thread_lifecycle_contributors() {
        contributor
            .on_thread_stop(codex_extension_api::ThreadStopInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
            })
            .await;
    }
}

async fn persist_completion_mailbox_before_shutdown(
    sess: &Arc<Session>,
) -> Result<(), ThreadStoreError> {
    loop {
        let transition = sess.active_turn_transition.notified();
        tokio::pin!(transition);
        transition.as_mut().enable();
        let active_task = sess
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|active_turn| active_turn.task.is_some());
        match active_task {
            None => break,
            Some(true) => {
                sess.abort_all_tasks(TurnAbortReason::Replaced).await;
            }
            Some(false) => transition.as_mut().await,
        }
    }
    let persistence_barrier =
        sess.durable_context_lock
            .acquire()
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to wait for completion context recording: {err}"),
            })?;
    drop(persistence_barrier);
    let turn_context = sess.new_default_turn().await;
    let mut attempt = 1;
    loop {
        let completion_drain = sess
            .input_queue
            .drain_completion_communications_for_shutdown()
            .await;
        if completion_drain.communications.is_empty() && !completion_drain.has_committing {
            return Ok(());
        }
        let mut deferred = completion_drain.has_committing;
        for communication in completion_drain.communications {
            match sess
                .record_inter_agent_communication(Arc::clone(&turn_context), communication)
                .await
            {
                InterAgentCommunicationRecord::CompletionRecorded => {}
                InterAgentCommunicationRecord::CompletionDeferred => {
                    deferred = true;
                    break;
                }
                InterAgentCommunicationRecord::Ordinary => {
                    return Err(ThreadStoreError::Internal {
                        message: "shutdown completion drain returned ordinary communication"
                            .to_string(),
                    });
                }
            }
        }
        if deferred {
            let delay = crate::util::backoff(attempt).min(Duration::from_secs(5));
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(delay).await;
        }
    }
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    shutdown_session_runtime(sess).await;
    if let Err(err) = persist_completion_mailbox_before_shutdown(sess).await {
        warn!("failed to persist accepted completion context before shutdown: {err}");
    }
    info!("Shutting down Codex instance");
    let history = sess.clone_history().await;
    let turn_count = history
        .raw_items()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    emit_thread_stop_lifecycle(sess.as_ref()).await;

    // Gracefully flush and shutdown thread persistence on session end so tests
    // that inspect durable state do not race with the background writer.
    if let Some(live_thread) = sess.live_thread()
        && let Err(e) = live_thread.shutdown().await
    {
        warn!("failed to shutdown thread persistence: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: "Failed to shutdown thread persistence".to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }

    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    sess.services
        .rollout_thread_trace
        .record_protocol_event(&event.msg);
    sess.deliver_event_raw(event).await;
    sess.services
        .rollout_thread_trace
        .record_ended(codex_rollout_trace::RolloutStatus::Completed);
    true
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess
        .new_turn_with_default_settings(sub_id.clone(), Default::default())
        .await;
    sess.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;
    #[allow(deprecated)]
    match resolve_review_request(review_request, &turn_context.cwd) {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    // To break out of this loop, send Op::Shutdown.
    let mut shutdown_received = false;
    let mut reload_required = false;
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        let dispatch_span = submission_dispatch_span(&sub);
        let should_exit = async {
            match sub.op {
                Op::Interrupt => {
                    interrupt(&sess).await;
                    false
                }
                Op::CleanBackgroundTerminals => {
                    clean_background_terminals(&sess).await;
                    false
                }
                Op::RealtimeConversationStart(params) => {
                    if let Err(err) =
                        handle_realtime_conversation_start(&sess, sub.id.clone(), params).await
                    {
                        sess.send_event_raw(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Error(ErrorEvent {
                                misalignment: None,
                                message: err.to_string(),
                                codex_error_info: Some(CodexErrorInfo::Other),
                            }),
                        })
                        .await;
                    }
                    false
                }
                Op::RealtimeConversationAudio(params) => {
                    handle_realtime_conversation_audio(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationText(params) => {
                    handle_realtime_conversation_text(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationSpeech(params) => {
                    handle_realtime_conversation_speech(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationClose => {
                    handle_realtime_conversation_close(&sess, sub.id.clone()).await;
                    false
                }
                Op::RealtimeConversationListVoices => {
                    realtime_conversation_list_voices(&sess, sub.id.clone()).await;
                    false
                }
                Op::TurnInput {
                    request,
                    mode,
                    reply,
                } => {
                    let result = turn_input::handle(&sess, *request, mode, sub.id.clone()).await;
                    let _ = reply.send(result);
                    false
                }
                Op::UserInput { .. } | Op::AgentInput { .. } => {
                    user_input_or_turn(
                        &sess,
                        sub.id.clone(),
                        sub.op,
                        /*client_user_message_id*/ None,
                        sub.parent_turn_id,
                        sub.root_turn_id,
                    )
                    .await;
                    false
                }
                Op::RecoverTurn {
                    thread_settings,
                    start_options,
                    reply,
                } => {
                    let result = turn_input::handle_recovery(
                        &sess,
                        thread_settings,
                        start_options,
                        sub.id.clone(),
                    )
                    .await;
                    let _ = reply.send(result);
                    false
                }
                Op::SuspendTurnAndShutdown { reply } => {
                    let result =
                        super::turn_suspension::suspend_turn_and_shutdown(&sess, sub.id.clone())
                            .await;
                    // Exit only after history is durable and its writer has closed; an error
                    // must leave responsibility for the thread with the current worker.
                    let should_exit = matches!(
                        &result,
                        Ok(codex_protocol::turn_input::SuspendTurnOutcome::Suspended { .. })
                    );
                    let _ = reply.send(result);
                    should_exit
                }
                Op::ThreadSettings { thread_settings } => {
                    thread_settings::update(&sess, sub.id.clone(), thread_settings).await;
                    false
                }
                Op::TurnSettings {
                    turn_id,
                    update,
                    reply,
                } => {
                    let outcome = sess.apply_turn_settings(&turn_id, update).await;
                    let _ = reply.send(outcome);
                    false
                }
                Op::InterAgentCommunication {
                    communication,
                    start_options,
                } => {
                    inter_agent_communication(&sess, sub.id.clone(), communication, start_options)
                        .await;
                    false
                }
                Op::ExecApproval {
                    id: approval_id,
                    turn_id,
                    decision,
                } => {
                    exec_approval(&sess, approval_id, turn_id, decision).await;
                    false
                }
                Op::PatchApproval { id, decision } => {
                    patch_approval(&sess, id, decision).await;
                    false
                }
                Op::UserInputAnswer { id, response } => {
                    request_user_input_response(&sess, id, response).await;
                    false
                }
                Op::RequestPermissionsResponse { id, response } => {
                    request_permissions_response(&sess, id, response).await;
                    false
                }
                Op::DynamicToolResponse { id, response } => {
                    dynamic_tool_response(&sess, id, response).await;
                    false
                }
                Op::RefreshMcpServers => {
                    refresh_mcp_servers(&sess);
                    false
                }
                Op::ActivateMcpServer { server_name } => {
                    queue_mcp_server_use_context(&sess, server_name).await;
                    false
                }
                Op::ReloadUserConfig => {
                    reload_user_config(&sess).await;
                    false
                }
                Op::Compact => {
                    compact(&sess, sub.id.clone()).await;
                    false
                }
                Op::ThreadRollback { num_turns } => {
                    let disposition = thread_rollback_target(
                        &sess,
                        sub.id.clone(),
                        ThreadRollbackTarget::InstructionTurns(num_turns),
                    )
                    .await;
                    reload_required =
                        matches!(disposition, ThreadRollbackDisposition::ReloadRequired);
                    reload_required
                }
                Op::ThreadRollbackMaterialized {
                    num_turns,
                    expected_start_turn_id,
                    expected_turn_count,
                } => {
                    let disposition = thread_rollback_target(
                        &sess,
                        sub.id.clone(),
                        ThreadRollbackTarget::MaterializedTurns {
                            num_turns,
                            expected_start_turn_id,
                            expected_turn_count,
                        },
                    )
                    .await;
                    reload_required =
                        matches!(disposition, ThreadRollbackDisposition::ReloadRequired);
                    reload_required
                }
                Op::SetThreadMemoryMode { mode } => {
                    set_thread_memory_mode(&sess, sub.id.clone(), mode).await;
                    false
                }
                Op::RunUserShellCommand {
                    command,
                    timeout_ms,
                } => {
                    run_user_shell_command(&sess, sub.id.clone(), command, timeout_ms).await;
                    false
                }
                Op::ResolveElicitation {
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                } => {
                    resolve_elicitation(&sess, server_name, request_id, decision, content, meta)
                        .await;
                    false
                }
                Op::Shutdown => shutdown(&sess, sub.id.clone()).await,
                Op::Review { review_request } => {
                    review(&sess, &config, sub.id.clone(), review_request).await;
                    false
                }
                Op::ApproveGuardianDeniedAction { event } => {
                    approve_guardian_denied_action(&sess, event).await;
                    false
                }
                _ => false, // Ignore unknown ops; enum is non_exhaustive to allow extensions.
            }
        }
        .instrument(dispatch_span)
        .await;
        if should_exit {
            // Submission admission switches to reload-required before an indeterminate rollback
            // error is delivered. Closing the receiver here finalizes that quarantine and ensures
            // no work runs against live context that may disagree with durable history.
            rx_sub.close();
            shutdown_received = !reload_required;
            break;
        }
    }
    // If the submission loop exits because the channel closed without an
    // explicit shutdown op, still run session teardown.
    if !shutdown_received {
        if reload_required
            && let Some(live_thread) = sess.live_thread()
            && let Err(err) = live_thread.shutdown().await
        {
            warn!(
                "failed to shut down thread persistence before reloading indeterminate history: {err}"
            );
            if let Err(discard_err) = live_thread.discard().await {
                warn!(
                    "failed to discard thread persistence after reload shutdown failed: {discard_err}"
                );
            }
        }
        shutdown_session_runtime(&sess).await;
        emit_thread_stop_lifecycle(sess.as_ref()).await;
        if !reload_required
            && let Some(live_thread) = sess.live_thread()
            && let Err(err) = live_thread.shutdown().await
        {
            warn!("failed to shutdown thread persistence after submission channel closed: {err}");
        }
    }
    debug!("Agent loop exited");
}

async fn approve_guardian_denied_action(sess: &Arc<Session>, event: GuardianAssessmentEvent) {
    if event.status != GuardianAssessmentStatus::Denied {
        warn!(
            review_id = event.id.as_str(),
            "ignoring approval for non-denied Guardian assessment"
        );
        return;
    }

    let approved_action = serde_json::json!({
        "action": &event.action,
        "outcome": "allowed",
    });
    let approved_action_json = match serde_json::to_string_pretty(&approved_action) {
        Ok(approved_action_json) => approved_action_json,
        Err(error) => {
            warn!(%error, review_id = event.id.as_str(), "failed to serialize approved Guardian action");
            return;
        }
    };
    let items = vec![ContextualUserFragment::into(GuardianApprovedAction::new(
        approved_action_json,
    ))];

    sess.inject_no_new_turn(items, /*current_turn_context*/ None)
        .await;
}

pub(super) fn submission_dispatch_span(sub: &Submission) -> tracing::Span {
    let op_name = sub.op.kind();
    let span_name = format!("op.dispatch.{op_name}");
    let dispatch_span = match &sub.op {
        Op::RealtimeConversationAudio(_) => {
            debug_span!(
                "submission_dispatch",
                otel.name = span_name.as_str(),
                submission.id = sub.id.as_str(),
                codex.op = op_name
            )
        }
        _ => info_span!(
            "submission_dispatch",
            otel.name = span_name.as_str(),
            submission.id = sub.id.as_str(),
            codex.op = op_name
        ),
    };
    if let Some(trace) = sub.trace.as_ref()
        && !set_parent_from_w3c_trace_context(&dispatch_span, trace)
    {
        warn!(
            submission.id = sub.id.as_str(),
            "ignoring invalid submission trace carrier"
        );
    }
    dispatch_span
}
