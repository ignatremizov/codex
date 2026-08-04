//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Accepted delivery retains the exact parent runtime through canonical publication.

use super::LocalAgentControl;
use crate::agent::api::AgentTurnOutcome;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadTraceContext;
use tracing::debug;

impl LocalAgentControl {
    /// Routes a captured terminal outcome without retaining the child's live turn context.
    pub(crate) async fn notify_parent_of_terminal_turn(
        &self,
        outcome: AgentTurnOutcome,
        terminal: super::AgentTerminalPresentation,
        trace: &ThreadTraceContext,
    ) {
        let control = self.clone();
        let trace = trace.clone();
        tokio::spawn(async move {
            let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                agent_path: Some(child_agent_path),
                ..
            }) = &outcome.source
            else {
                control.claim_completion_context_response_item_id(
                    terminal.parent(),
                    &terminal.completion_context_response_item_id(),
                );
                return;
            };
            let parent_thread_id = *parent_thread_id;
            let status = outcome.status.clone();
            let Some(parent_agent_path) = child_agent_path
                .as_str()
                .rsplit_once('/')
                .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
            else {
                control.claim_completion_context_response_item_id(
                    terminal.parent(),
                    &terminal.completion_context_response_item_id(),
                );
                return;
            };

            if matches!(status, AgentStatus::Completed(_))
                && let Some(parent_turn_id) = outcome.parent_turn_id.clone()
            {
                let initiating_thread_id = match outcome.initiating_agent_path.as_ref() {
                    Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                        control.resolve_agent_reference(
                            outcome.thread_id,
                            &outcome.source,
                            initiating_agent_path.as_str(),
                        )
                        .await
                        .inspect_err(|err| {
                            debug!(
                                "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                            );
                        })
                        .ok()
                    }
                    _ => Some(parent_thread_id),
                };
                if let Some(initiating_thread_id) = initiating_thread_id
                    && let Err(err) = control
                        .emit_sub_agent_activity(
                            initiating_thread_id,
                            parent_turn_id,
                            SubAgentActivityItem {
                                id: format!("subagent-completed-{}", outcome.turn_id),
                                kind: SubAgentActivityKind::Completed,
                                agent_thread_id: outcome.thread_id,
                                agent_path: child_agent_path.clone(),
                                prompt: None,
                            },
                        )
                        .await
                {
                    debug!(
                        "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                    );
                }
            }

            let reference = child_agent_path.to_string();
            let path = child_agent_path.clone();
            control.deliver_terminal_completion(outcome, terminal, &reference, Some(path), &trace);
        });
    }
}

impl LocalAgentControl {
    pub(super) fn deliver_terminal_completion(
        &self,
        outcome: AgentTurnOutcome,
        terminal: super::AgentTerminalPresentation,
        reference: &str,
        child_path: Option<AgentPath>,
        trace: &ThreadTraceContext,
    ) {
        let control = self.clone();
        let reference = reference.to_string();
        let trace = trace.clone();
        tokio::spawn(async move {
            if control
                .await_response_observation_event_match(
                    terminal.parent(),
                    terminal.child(),
                    &outcome.turn_id,
                )
                .await
            {
                return;
            }
            control.deliver_native_terminal_completion(
                outcome, terminal, &reference, child_path, &trace,
            );
        });
    }

    fn deliver_native_terminal_completion(
        &self,
        outcome: AgentTurnOutcome,
        terminal: super::AgentTerminalPresentation,
        reference: &str,
        child_path: Option<AgentPath>,
        trace: &ThreadTraceContext,
    ) {
        let Some(parent) = terminal.take_parent_thread() else {
            return;
        };
        let Some(reservation) = terminal.take_accepted_completion_delivery() else {
            return;
        };
        let control = self.clone();
        let reference = reference.to_string();
        let trace = trace.clone();
        tokio::spawn(async move {
            let parent_id = terminal.parent();
            let context_id = terminal.completion_context_response_item_id();
            let result = async {
                let communication = child_path.and_then(|child_path| {
                    let parent_path = child_path
                        .as_str()
                        .rsplit_once('/')
                        .and_then(|(parent, _)| AgentPath::try_from(parent).ok())?;
                    let message = format_inter_agent_completion_message(
                        parent_path.clone(),
                        child_path.clone(),
                        &outcome.status,
                    )?;
                    let mut communication = InterAgentCommunication::new(
                        child_path,
                        parent_path,
                        Vec::new(),
                        message,
                        /*trigger_turn*/ false,
                    );
                    communication.id = Some(context_id.clone());
                    Some(communication)
                });
                let response = match &communication {
                    Some(communication) => communication.to_model_input_item(),
                    None => codex_protocol::models::ResponseItem::Message {
                        id: Some(context_id.clone()),
                        role: "user".to_string(),
                        phase: None,
                        content: vec![codex_protocol::models::ContentItem::InputText {
                            text: crate::session_prefix::format_subagent_notification_message(
                                &reference,
                                outcome.thread_id,
                                &outcome.status,
                            ),
                        }],
                        internal_chat_message_metadata_passthrough: None,
                    },
                };
                let delivery = if communication.is_some() {
                    crate::session::CompletionContextDelivery::QueueOnly
                } else {
                    crate::session::CompletionContextDelivery::InstallNow
                };
                let has_mailbox_context = communication.is_some();
                parent
                    .session
                    .persist_completion_context(response, &reservation, delivery)
                    .await?;
                if let Some(communication) = communication {
                    parent
                        .session
                        .input_queue
                        .enqueue_mailbox_communication(
                            communication,
                            crate::TurnStartOptions::default(),
                        )
                        .await;
                }
                if !terminal.wait_owns_presentation().await {
                    parent
                        .session
                        .publish_completion_item(terminal.completion_presentation(), &reservation)
                        .await?;
                }
                if trace.is_enabled() {
                    trace.record_agent_result_interaction(
                        &outcome.turn_id,
                        parent_id.thread_id,
                        &AgentResultTracePayload {
                            child_agent_path: &reference,
                            message: &crate::session_prefix::format_subagent_notification_message(
                                &reference,
                                outcome.thread_id,
                                &outcome.status,
                            ),
                            status: &outcome.status,
                        },
                    );
                }
                if !has_mailbox_context {
                    control.claim_completion_context_response_item_id(parent_id, &context_id);
                }
                Ok::<_, codex_protocol::error::CodexErr>(())
            }
            .await;
            if let Err(error) = result {
                parent
                    .session
                    .quarantine_history(format!("completion delivery failed: {error}"));
                debug!("completion delivery remains unresolved: {error}");
                control.claim_completion_context_response_item_id(parent_id, &context_id);
            }
            drop(reservation);
        });
    }
}
