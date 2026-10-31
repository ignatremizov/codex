//! Resume through the selected controller without admitting a prompt or response subscription.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::agent::api::AgentTarget;
use crate::agent::api::ResumeRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::tools::handlers::multi_agents_resume_spec::create_resume_agent_tool_v2;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("resume_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_resume_agent_tool_v2()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let mut analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::ResumeAgent);
            let result = handle_resume_agent(invocation, &mut analytics).await;
            analytics.finish(&result);
            result.map(boxed_tool_output)
        })
    }
}

async fn handle_resume_agent(
    invocation: ToolInvocation,
    analytics: &mut ToolCallAnalytics,
) -> Result<ResumeAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    let args: ResumeAgentArgs = parse_arguments(&function_arguments(payload)?)?;
    let agent_id = resolve_agent_target(&session, &turn, &args.target).await?;
    analytics.set_receiver(agent_id);
    let snapshot = session
        .services
        .agent_control
        .resume(ResumeRequest {
            caller: session.thread_id,
            target: AgentTarget::Id(agent_id),
            resume_config: build_agent_resume_config(&turn)
                .map_err(FunctionCallError::RespondToModel)?,
            model: args.model,
            reasoning_effort: args.reasoning_effort,
        })
        .await
        .map_err(|error| collab_v2_agent_error(agent_id, error))?;
    let task_name = snapshot
        .metadata()
        .agent_path
        .as_ref()
        .map(ToString::to_string)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel("target agent is missing an agent_path".into())
        })?;
    let status = snapshot.status().cloned().ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent was unloaded during resume; retry".into())
    })?;
    Ok(ResumeAgentResult { task_name, status })
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeAgentArgs {
    target: String,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Serialize)]
struct ResumeAgentResult {
    task_name: String,
    status: AgentStatus,
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
