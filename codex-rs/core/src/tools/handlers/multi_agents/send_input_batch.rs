//! Sequential array-target admission. Cancellation keeps the ordinary tool abort semantics.
//!
//! An error can follow admission. Neither errors nor cancellation authorize replay of the batch.

use super::send_input::SendInputAdmissionStatus;
use super::send_input::SendInputMode;
use super::send_input::SendInputResult;
use super::send_input_admission::RecipientInput;
use super::send_input_admission::admit_input;
use super::send_input_admission::prepare_receiver;
use super::send_input_admission::resolve_receiver;
use super::*;
use crate::agent::control::render_input_preview;
use codex_protocol::CollabAgentInputBatch;
use codex_protocol::CollabAgentInputResult;
use codex_protocol::CollabAgentInputStatus;
use std::collections::HashSet;

pub(super) async fn handle_batch(
    invocation: ToolInvocation,
    targets: Vec<String>,
    items: Vec<UserInput>,
    mode: SendInputMode,
    interrupt: bool,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        call_id,
        ..
    } = invocation;
    let mut seen = HashSet::new();
    let mut receivers = Vec::new();
    let mut receiver_agents = Vec::new();
    // Freeze identities before admission, deduplicating aliases rather than authored strings.
    for target in targets {
        let receiver = resolve_receiver(&session, &target, mode).await;
        if let Ok(id) = &receiver
            && !seen.insert(*id)
        {
            continue;
        }
        if let Ok(id) = &receiver
            && let Ok(agent) = session
                .services
                .agent_control
                .get_agent_presentation_ref(*id)
                .await
        {
            receiver_agents.push(agent);
        }
        receivers.push((target, receiver));
    }
    let observation = mode.response_observation();
    let mailbox = matches!(mode, SendInputMode::Mailbox(_));
    let mut item = CollabAgentToolCallItem {
        id: call_id.clone(),
        tool: CollabAgentTool::SendInput,
        status: CollabAgentToolCallStatus::InProgress,
        observe_commentary: Some(observation.commentary()),
        wake_on_completion: observation.wake_on_completion_item_value(),
        target_messages: Some(observation.target_messages()),
        queue_input: Some(observation.queue_input()),
        mailbox_input: mailbox.then_some(true),
        input_batch: Some(CollabAgentInputBatch {
            flags: mode.normalized_flags(),
            results: Vec::new(),
        }),
        deadline_at_ms: None,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: receivers
            .iter()
            .filter_map(|(_, id)| id.as_ref().ok().copied())
            .collect(),
        receiver_agents,
        prompt: Some(render_input_preview(&items)),
        model: None,
        reasoning_effort: None,
        agents_states: Default::default(),
        completion_presentation_agent_ids: None,
    };
    session
        .emit_turn_item_started(&turn, &TurnItem::CollabAgentToolCall(item.clone()))
        .await;
    let mut results = Vec::new();
    let mut admissions = Vec::new();
    for (target, receiver) in receivers {
        let mut entry = CollabAgentInputResult {
            target: receiver
                .as_ref()
                .ok()
                .map(|id| {
                    item.receiver_agents
                        .iter()
                        .find(|agent| agent.thread_id == *id)
                        .and_then(|agent| agent.agent_ref.clone())
                        .unwrap_or_else(|| id.to_string())
                })
                .unwrap_or(target),
            receiver_thread_id: receiver.as_ref().ok().map(ToString::to_string),
            status: CollabAgentInputStatus::Error,
            error: None,
            hint: None,
        };
        let outcome = async {
            let receiver = receiver?;
            let prepared = prepare_receiver(&session, &turn, receiver, mode, interrupt).await?;
            entry.target = prepared
                .agent
                .agent_ref
                .clone()
                .unwrap_or_else(|| receiver.to_string());
            if let Some(agent) = item
                .receiver_agents
                .iter_mut()
                .find(|agent| agent.thread_id == receiver)
            {
                *agent = prepared.agent.clone();
            } else {
                item.receiver_agents.push(prepared.agent.clone());
            }
            let mut result = admit_input(
                &session,
                &turn,
                RecipientInput {
                    receiver: &prepared,
                    call_id: &call_id,
                    items: items.clone(),
                    mode,
                },
            )
            .await;
            let status = session.services.agent_control.get_status(receiver).await;
            if mailbox {
                if matches!(status, AgentStatus::NotFound)
                    && let Ok(result) = &mut result
                {
                    result.hint = Some(
                        "Mail saved; receiver not loaded. Use resume_agent first.".to_string(),
                    );
                }
            } else {
                item.agents_states.insert(receiver, status);
            }
            result
        }
        .await;
        match outcome {
            Ok(result) => {
                entry.status = match result.status {
                    SendInputAdmissionStatus::Submitted => CollabAgentInputStatus::Submitted,
                    SendInputAdmissionStatus::Queued => CollabAgentInputStatus::Queued,
                    SendInputAdmissionStatus::MailboxAccepted => {
                        CollabAgentInputStatus::MailboxAccepted
                    }
                };
                entry.hint = result.hint.clone();
                admissions.push(result);
            }
            Err(error) => entry.error = Some(error.to_string()),
        }
        results.push(entry);
    }
    let output = BatchOutput {
        results: results
            .iter()
            .map(|result| ModelRecipientResult {
                target: result.target.clone(),
                status: result.status,
                error: result.error.clone(),
                hint: result.hint.clone(),
            })
            .collect(),
        admissions,
    };
    item.status = if output.success_for_logging() {
        CollabAgentToolCallStatus::Completed
    } else {
        CollabAgentToolCallStatus::Failed
    };
    item.input_batch = Some(CollabAgentInputBatch {
        flags: mode.normalized_flags(),
        results,
    });
    session
        .emit_turn_item_completed(&turn, TurnItem::CollabAgentToolCall(item))
        .await;
    Ok(boxed_tool_output(output))
}

#[derive(Serialize)]
struct ModelRecipientResult {
    target: String,
    status: CollabAgentInputStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

#[derive(Serialize)]
struct BatchOutput {
    results: Vec<ModelRecipientResult>,
    admissions: Vec<SendInputResult>,
}

#[derive(Serialize)]
struct BatchModelResult<'a> {
    results: &'a [ModelRecipientResult],
}

impl ToolOutput for BatchOutput {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_input")
    }

    fn success_for_logging(&self) -> bool {
        self.results
            .iter()
            .all(|result| result.status != CollabAgentInputStatus::Error)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            &BatchModelResult {
                results: &self.results,
            },
            Some(self.success_for_logging()),
            "send_input",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(
            &BatchModelResult {
                results: &self.results,
            },
            "send_input",
        )
    }
}
