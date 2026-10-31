//! Surface live subagent questions and submit answers using their captured routing identity.

use super::*;
use crate::agent_question::AgentQuestionAnswer;
use crate::agent_question::AgentQuestionAnswerOutcome;
use crate::agent_question::AgentQuestionBinding;
use crate::agent_question::AgentQuestionDelivery;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::AgentControlAction;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentControlParams;
use codex_app_server_protocol::AgentControlResponse;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentInputOutcome;
use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::UserInput;
use codex_protocol::items::AgentMessageDelivery;
use uuid::Uuid;

pub(super) fn is_native_async_question(notification: &ServerNotification) -> bool {
    matches!(notification,
        ServerNotification::ItemCompleted(completed)
            if matches!(&completed.item.payload, ThreadItemPayload::AgentMessage {
                questions: Some(_),
                delivery: Some(AgentMessageDelivery::Async),
                attribution: None,
                inter_agent_source: None,
                ..
            })
    )
}

impl App {
    pub(super) fn track_agent_questions(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) {
        let ServerNotification::ItemCompleted(completed) = notification else {
            return;
        };
        // A live spawn can introduce a child without ThreadStarted or a picker refresh.
        // Record its trusted relationship before the child's first question can arrive.
        if let ThreadItemPayload::CollabAgentToolCall {
            tool: codex_app_server_protocol::CollabAgentTool::SpawnAgent,
            status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
            sender_thread_id,
            receiver_thread_ids,
            receiver_agents,
            ..
        } = &completed.item.payload
            && *sender_thread_id == thread_id.to_string()
        {
            for target in receiver_thread_ids {
                let Ok(target_id) = ThreadId::from_string(target) else {
                    continue;
                };
                if target_id == thread_id
                    || self.agent_navigation.alias(target_id).is_some_and(|alias| {
                        alias.state == codex_app_server_protocol::AgentAliasState::Transferred
                            || alias.agent_ref == 1
                    })
                {
                    continue;
                }
                let metadata = receiver_agents
                    .iter()
                    .find(|agent| agent.thread_id == *target);
                self.agent_navigation.mark_subagent(target_id);
                self.agent_navigation
                    .set_parent_thread_id(target_id, Some(thread_id));
                self.agent_navigation.upsert(
                    target_id,
                    metadata.and_then(|agent| agent.agent_nickname.clone()),
                    metadata.and_then(|agent| agent.agent_role.clone()),
                    /*is_closed*/ false,
                );
            }
            return;
        }
        // Prefer the requester's known owner over the selected conversation as control
        // authority. Receipt matching itself needs only backend/requester identity.
        let source_thread_id = self
            .agent_navigation
            .parent_thread_id(thread_id)
            .filter(|source| *source != thread_id)
            .or_else(|| {
                self.agent_navigation
                    .root_thread_id()
                    .filter(|source| *source != thread_id)
            })
            .or_else(|| {
                self.current_displayed_thread_id()
                    .filter(|source| *source != thread_id)
            })
            .unwrap_or(thread_id);
        let binding = AgentQuestionBinding {
            backend_id: app_server.backend_id(),
            source_thread_id,
            requester_thread_id: thread_id,
            requester_label: self.thread_label(thread_id),
        };
        match &completed.item.payload {
            ThreadItemPayload::AgentMessage {
                id,
                questions: Some(questions),
                delivery: Some(AgentMessageDelivery::Async),
                attribution: None,
                inter_agent_source: None,
                ..
            } if source_thread_id != thread_id
                && self
                    .agent_navigation
                    .parent_thread_id(thread_id)
                    .is_some_and(|parent| self.owns_thread_for_routing(parent))
                && self.agent_navigation.alias(thread_id).is_none_or(|alias| {
                    alias.state != codex_app_server_protocol::AgentAliasState::Transferred
                        && alias.agent_ref != 1
                }) =>
            {
                self.chat_widget.add_agent_questions(binding, id, questions);
            }
            ThreadItemPayload::UserMessage { content, .. } => {
                if let Some(replies) = crate::async_question_reply::parse_input(content) {
                    let ids = replies
                        .into_iter()
                        .map(|reply| reply.question_item_id)
                        .collect::<Vec<_>>();
                    self.chat_widget
                        .resolve_agent_question_answers(binding, &ids);
                }
            }
            _ => {}
        }
    }

    pub(super) async fn answer_agent_question(
        &mut self,
        app_server: &mut AppServerSession,
        answer: AgentQuestionAnswer,
    ) {
        if !self.chat_widget.has_pending_agent_answer(&answer) {
            return;
        }
        let binding = &answer.binding;
        let validation = if self.reconnect.offline {
            Err(color_eyre::eyre::eyre!(
                "Reconnect before answering; your answer is preserved"
            ))
        } else if binding.backend_id != app_server.backend_id() {
            Err(color_eyre::eyre::eyre!(
                "The question belongs to another server; reconnect to it before answering"
            ))
        } else {
            self.ensure_agent_control_admission(
                binding.source_thread_id,
                Some(binding.requester_thread_id),
            )
            .await
        };
        if let Err(error) = validation {
            self.chat_widget
                .finish_agent_question_answer(&answer, AgentQuestionAnswerOutcome::Rejected);
            self.chat_widget.add_error_message(error.to_string());
            return;
        }
        // The payload is a literal user answer. Do not parse commands, expand mentions, change
        // the selected thread, or give the source model a subscription to the target's response.
        let input = vec![UserInput::Text {
            text: answer.text.clone(),
            text_elements: Vec::new(),
        }];
        let target = binding.requester_thread_id.to_string();
        let response_handling = Some(AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::None,
            /*target_messages*/ false,
            /*queue_input*/ false,
        ));
        let action = match answer.delivery {
            AgentQuestionDelivery::Direct => AgentControlAction::Prompt {
                target,
                input,
                response_handling,
            },
            AgentQuestionDelivery::Queued => AgentControlAction::QueuedPrompt {
                target,
                input,
                response_handling,
            },
        };
        let result = app_server
            .request_handle()
            .request_typed::<AgentControlResponse>(ClientRequest::AgentControl {
                request_id: RequestId::String(format!("agent-question-{}", Uuid::new_v4())),
                params: AgentControlParams {
                    source_thread_id: binding.source_thread_id.to_string(),
                    // Clicking an answer supplies routing authority, not an authored selector.
                    authored_selector: None,
                    action,
                },
            })
            .await;
        let mut outcome = AgentQuestionAnswerOutcome::Uncertain;
        match result {
            Ok(response) => {
                if let AgentControlOutcome::Prompted {
                    target_thread_id,
                    input_outcome,
                    post_admission_warning,
                    ..
                } = response.outcome
                    && ThreadId::from_string(&target_thread_id).ok()
                        == Some(binding.requester_thread_id)
                {
                    if input_outcome != AgentInputOutcome::Unknown {
                        outcome = AgentQuestionAnswerOutcome::Accepted;
                    }
                    if input_outcome == AgentInputOutcome::Queued {
                        self.app_event_tx.send(AppEvent::RefreshAgentPromptQueue);
                        self.chat_widget.add_info_message(
                            format!("Answer queued for {}.", binding.requester_label),
                            /*hint*/ None,
                        );
                    }
                    if let Some(warning) = post_admission_warning {
                        self.chat_widget.add_error_message(format!(
                            "Answer for {} has a delivery warning: {warning}",
                            binding.requester_label
                        ));
                    }
                }
                if let Some(warning) = response.audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Answer audit warning; do not send it again: {warning}"
                    ));
                }
            }
            Err(error) => {
                if matches!(
                    &error,
                    TypedRequestError::Server { source, .. }
                        if matches!(source.code, -32602..=-32600)
                ) {
                    outcome = AgentQuestionAnswerOutcome::Rejected;
                }
                self.chat_widget.add_error_message(format!(
                    "Could not answer {}: {error:#}",
                    binding.requester_label
                ));
            }
        }
        self.chat_widget
            .finish_agent_question_answer(&answer, outcome);
        if outcome == AgentQuestionAnswerOutcome::Uncertain {
            self.chat_widget.add_error_message(format!(
                "Answer delivery to {} is uncertain. Check that conversation before sending it again; your answer is preserved.",
                binding.requester_label
            ));
        }
    }
}
