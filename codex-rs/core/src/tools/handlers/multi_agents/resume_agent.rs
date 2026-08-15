use super::*;
use crate::agent::agent_resolver::resolve_resumable_v1_agent_target;
use crate::agent::control::AgentResumeOwnership;
use crate::agent::next_thread_spawn_depth;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents_spec::create_resume_agent_tool;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolSpec;
use std::sync::Arc;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "resume_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_resume_agent_tool()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "resume_agent resume reopen closed agent subagent thread id target",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_resume_agent(invocation).await.map(boxed_tool_output) })
    }
}

async fn handle_resume_agent(
    invocation: ToolInvocation,
) -> Result<ResumeAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: ResumeAgentArgs = parse_arguments(&arguments)?;
    let receiver_thread_id = resolve_resumable_v1_agent_target(&session, &args.id).await?;
    if receiver_thread_id == session.thread_id {
        return Err(FunctionCallError::RespondToModel(
            "an agent cannot resume itself; continue the current turn directly".to_string(),
        ));
    }
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(receiver_thread_id)
        .unwrap_or_default();
    let resume_plan = session
        .services
        .agent_control
        .plan_agent_resume(receiver_thread_id)
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    let child_depth = next_thread_spawn_depth(&turn.session_source);
    if resume_plan.ownership.transfers_ownership()
        && exceeds_thread_spawn_depth_limit(child_depth, turn.config.agent_max_depth)
    {
        return Err(FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string(),
        ));
    }
    let mut status = resume_plan.status;
    let was_not_found = matches!(status, AgentStatus::NotFound);
    let task_name = resume_plan
        .ownership
        .transfers_ownership()
        .then(|| format!("model_adopt_{}", uuid::Uuid::now_v7().as_simple()));
    let resumed_session_source = thread_spawn_source(
        session.thread_id(),
        &turn.session_source,
        child_depth,
        /*agent_role*/ None,
        task_name,
    )?;
    let mut live_adoption_error = None;
    if !was_not_found {
        match session
            .services
            .agent_control
            .ensure_v1_completion_watcher(
                receiver_thread_id,
                resumed_session_source.clone(),
                args.w,
                status.clone(),
            )
            .await
        {
            Ok(adopted_status) => status = adopted_status,
            Err(err) => {
                live_adoption_error = Some(collab_agent_error(receiver_thread_id, err));
            }
        }
    }

    session
        .emit_turn_item_started(
            &turn,
            &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id.clone(),
                tool: CollabAgentTool::ResumeAgent,
                status: CollabAgentToolCallStatus::InProgress,
                observe_commentary: Some(args.w.commentary()),
                wake_on_completion: args.w.wake_on_completion_item_value(),
                target_messages: Some(args.w.target_messages()),
                queue_input: Some(args.w.queue_input()),
                deadline_at_ms: None,
                sender_thread_id: session.thread_id,
                receiver_thread_ids: vec![receiver_thread_id],
                receiver_agents: vec![CollabAgentRef {
                    thread_id: receiver_thread_id,
                    agent_nickname: receiver_agent.agent_nickname.clone(),
                    agent_role: receiver_agent.agent_role.clone(),
                }],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: Default::default(),
                completion_presentation_agent_ids: None,
            }),
        )
        .await;

    let (receiver_agent, mut error) = if was_not_found {
        match Box::pin(try_resume_closed_agent(
            &session,
            &turn,
            receiver_thread_id,
            resumed_session_source.clone(),
            resume_plan.ownership,
            args.id.clone(),
        ))
        .await
        {
            Ok(()) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                (
                    session
                        .services
                        .agent_control
                        .get_agent_metadata(receiver_thread_id)
                        .unwrap_or(receiver_agent),
                    None,
                )
            }
            Err(err) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                (receiver_agent, Some(err))
            }
        }
    } else {
        (receiver_agent, live_adoption_error)
    };
    if error.is_none() && was_not_found && !matches!(status, AgentStatus::NotFound) {
        match session
            .services
            .agent_control
            .ensure_v1_completion_watcher(
                receiver_thread_id,
                resumed_session_source,
                args.w,
                status.clone(),
            )
            .await
        {
            Ok(adopted_status) => status = adopted_status,
            Err(err) => error = Some(collab_agent_error(receiver_thread_id, err)),
        }
    }
    session
        .emit_turn_item_completed(
            &turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id,
                tool: CollabAgentTool::ResumeAgent,
                status: collab_tool_call_status(&status, Some(receiver_thread_id)),
                observe_commentary: Some(args.w.commentary()),
                wake_on_completion: args.w.wake_on_completion_item_value(),
                target_messages: Some(args.w.target_messages()),
                queue_input: Some(args.w.queue_input()),
                deadline_at_ms: None,
                sender_thread_id: session.thread_id(),
                receiver_thread_ids: vec![receiver_thread_id],
                receiver_agents: vec![CollabAgentRef {
                    thread_id: receiver_thread_id,
                    agent_nickname: receiver_agent.agent_nickname,
                    agent_role: receiver_agent.agent_role,
                }],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: [(receiver_thread_id, status.clone())].into_iter().collect(),
                completion_presentation_agent_ids: None,
            }),
        )
        .await;

    if let Some(err) = error {
        return Err(err);
    }
    turn.session_telemetry
        .counter("codex.multi_agent.resume", /*inc*/ 1, &[]);

    Ok(ResumeAgentResult { status })
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct ResumeAgentArgs {
    id: String,
    #[serde(default)]
    w: ResponseObservationPolicy,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ResumeAgentResult {
    pub(crate) status: AgentStatus,
}

impl ToolOutput for ResumeAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "resume_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "resume_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "resume_agent")
    }
}

async fn try_resume_closed_agent(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    receiver_thread_id: ThreadId,
    session_source: SessionSource,
    ownership: AgentResumeOwnership,
    authored_selector: String,
) -> Result<(), FunctionCallError> {
    let config = build_agent_resume_config(turn.as_ref())?;
    let result = match ownership {
        AgentResumeOwnership::CurrentRoot => {
            session
                .services
                .agent_control
                .resume_agent_from_rollout(
                    config,
                    receiver_thread_id,
                    session_source,
                    ResponseObservationPolicy::from_parts(
                        /*commentary*/ false,
                        FinalResponseObservation::None,
                    ),
                )
                .await
        }
        AgentResumeOwnership::Transfer {
            previous_session_id,
        } => {
            session
                .services
                .agent_control
                .resume_agent_from_rollout_adopting(
                    config,
                    receiver_thread_id,
                    session_source,
                    // The handler's post-resume adoption pass applies the requested policy once,
                    // including for standalone rollouts whose persisted source has no parent.
                    ResponseObservationPolicy::from_parts(
                        /*commentary*/ false,
                        FinalResponseObservation::None,
                    ),
                    previous_session_id,
                    authored_selector,
                )
                .await
        }
    };
    result
        .map(|_| ())
        .map_err(|err| collab_agent_error(receiver_thread_id, err))
}
