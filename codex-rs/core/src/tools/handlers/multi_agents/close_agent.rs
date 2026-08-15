use super::*;
use crate::agent::agent_resolver::resolve_resumable_v1_agent_target;
use crate::agent::control::CloseAgentResponseDisposition;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::tools::handlers::multi_agents_spec::create_close_agent_tool_v1;
use codex_protocol::error::CodexErrorDetails;
use codex_tools::ToolSpec;
use std::sync::Arc;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "close_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_close_agent_tool_v1()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "close_agent close shutdown stop agent subagent thread status target",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_close_agent(invocation).await.map(boxed_tool_output) })
    }
}

async fn handle_close_agent(
    invocation: ToolInvocation,
) -> Result<CloseAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: CloseAgentArgs = parse_arguments(&arguments)?;
    let requested_response_observation = args.w;
    let response_observation = requested_response_observation.unwrap_or_default();
    let observe_commentary = requested_response_observation.map(|_| false);
    let wake_on_completion = requested_response_observation
        .and_then(ResponseObservationPolicy::wake_on_completion_item_value);
    let target_messages = requested_response_observation.map(|_| false);
    let queue_input = requested_response_observation.map(ResponseObservationPolicy::queue_input);
    let agent_id = resolve_resumable_v1_agent_target(&session, &args.target).await?;
    if agent_id == session.thread_id {
        return Err(FunctionCallError::RespondToModel(
            "an agent cannot close itself; return your result instead".to_string(),
        ));
    }
    if agent_id == ThreadId::from(session.services.agent_control.session_id()) {
        return Err(FunctionCallError::RespondToModel(
            "a child agent cannot close Main".to_string(),
        ));
    }
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .unwrap_or_default();
    let close_response = session
        .services
        .agent_control
        .prepare_close_agent_response(
            Arc::clone(&session),
            codex_protocol::protocol::MultiAgentVersion::V1,
            agent_id,
        )
        .await
        .map_err(|err| collab_agent_error(agent_id, err))?;
    session
        .emit_turn_item_started(
            &turn,
            &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id.clone(),
                tool: CollabAgentTool::CloseAgent,
                status: CollabAgentToolCallStatus::InProgress,
                observe_commentary,
                wake_on_completion,
                target_messages,
                queue_input,
                deadline_at_ms: None,
                sender_thread_id: session.thread_id,
                receiver_thread_ids: vec![agent_id],
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: Default::default(),
                completion_presentation_agent_ids: None,
            }),
        )
        .await;
    let status = match session
        .services
        .agent_control
        .subscribe_status(agent_id)
        .await
    {
        Ok(mut status_rx) => status_rx.borrow_and_update().clone(),
        Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
            session.services.agent_control.get_status(agent_id).await
        }
        Err(err) => {
            let status = session.services.agent_control.get_status(agent_id).await;
            session
                .emit_turn_item_completed(
                    &turn,
                    TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                        id: call_id.clone(),
                        tool: CollabAgentTool::CloseAgent,
                        status: collab_tool_call_status(&status, Some(agent_id)),
                        observe_commentary,
                        wake_on_completion,
                        target_messages,
                        queue_input,
                        deadline_at_ms: None,
                        sender_thread_id: session.thread_id(),
                        receiver_thread_ids: vec![agent_id],
                        receiver_agents: vec![CollabAgentRef {
                            thread_id: agent_id,
                            agent_nickname: receiver_agent.agent_nickname.clone(),
                            agent_role: receiver_agent.agent_role.clone(),
                        }],
                        prompt: None,
                        model: None,
                        reasoning_effort: None,
                        agents_states: [(agent_id, status)].into_iter().collect(),
                        completion_presentation_agent_ids: None,
                    }),
                )
                .await;
            return Err(collab_agent_error(agent_id, err));
        }
    };
    let result = Box::pin(
        session
            .services
            .agent_control
            .close_agent_with_status(agent_id),
    )
    .await
    .map_err(|err| collab_agent_error(agent_id, err));
    let completed_status = result
        .as_ref()
        .map(|closed| closed.previous_status.clone())
        .unwrap_or_else(|_| status.clone());
    session
        .emit_turn_item_completed(
            &turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id,
                tool: CollabAgentTool::CloseAgent,
                status: collab_tool_call_status(&completed_status, Some(agent_id)),
                observe_commentary,
                wake_on_completion,
                target_messages,
                queue_input,
                deadline_at_ms: None,
                sender_thread_id: session.thread_id,
                receiver_thread_ids: vec![agent_id],
                receiver_agents: vec![CollabAgentRef {
                    thread_id: agent_id,
                    agent_nickname: receiver_agent.agent_nickname,
                    agent_role: receiver_agent.agent_role,
                }],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: [(agent_id, completed_status.clone())].into_iter().collect(),
                completion_presentation_agent_ids: None,
            }),
        )
        .await;
    let closed = result?;
    let response_delivery = close_response
        .deliver(&closed.previous_status, response_observation)
        .await;
    let previous_status = status_for_close_output(&closed.previous_status, response_delivery);

    Ok(CloseAgentResult {
        previous_status,
        response_delivery,
    })
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CloseAgentResult {
    pub(crate) previous_status: AgentStatus,
    pub(crate) response_delivery: CloseAgentResponseDisposition,
}

impl ToolOutput for CloseAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "close_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "close_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "close_agent")
    }
}

#[derive(Debug, Deserialize)]
struct CloseAgentArgs {
    target: String,
    w: Option<ResponseObservationPolicy>,
}

fn status_for_close_output(
    status: &AgentStatus,
    response_delivery: CloseAgentResponseDisposition,
) -> AgentStatus {
    match (status, response_delivery) {
        (
            AgentStatus::Completed(Some(_)),
            CloseAgentResponseDisposition::Suppressed
            | CloseAgentResponseDisposition::AlreadyVisible
            | CloseAgentResponseDisposition::Delivered
            | CloseAgentResponseDisposition::Queued
            | CloseAgentResponseDisposition::PresentationOnly,
        ) => AgentStatus::Completed(None),
        (
            AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Interrupted
            | AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound,
            CloseAgentResponseDisposition::NotApplicable,
        )
        | (
            AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Interrupted
            | AgentStatus::Completed(None)
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound,
            CloseAgentResponseDisposition::Suppressed
            | CloseAgentResponseDisposition::AlreadyVisible
            | CloseAgentResponseDisposition::Delivered
            | CloseAgentResponseDisposition::Queued
            | CloseAgentResponseDisposition::PresentationOnly,
        ) => status.clone(),
    }
}
