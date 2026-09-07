use super::list_agents_spec::create_list_agents_tool;
use super::*;
use crate::agent::control::AgentDirectoryPage;
use crate::agent::control::AgentDirectoryStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "list_agents")
    }

    fn spec(&self) -> ToolSpec {
        create_list_agents_tool()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "list_agents directory discover agents ref nickname role task path status",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            if turn.multi_agent_version != MultiAgentVersion::V1
                || !session
                    .services
                    .agent_control
                    .agent_directory_enabled(session.thread_id)
                    .await
                    .map_err(collab_spawn_error)?
            {
                return Err(FunctionCallError::RespondToModel(
                    "V1 list_agents is disabled. The user must explicitly enable \
                     tools.list_agents.enabled; messaging permission does not enable discovery."
                        .to_string(),
                ));
            }
            let arguments = function_arguments(payload)?;
            let args: ListAgentsArgs = parse_arguments(&arguments)?;
            let page = session
                .services
                .agent_control
                .list_agent_directory(
                    session.thread_id,
                    args.path_prefix.as_deref(),
                    args.status.unwrap_or(AgentDirectoryStatus::Loaded),
                    args.cursor.as_deref(),
                    args.limit,
                )
                .await
                .map_err(collab_spawn_error)?;

            Ok(boxed_tool_output(ListAgentsResult(page)))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    path_prefix: Option<String>,
    status: Option<AgentDirectoryStatus>,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
struct ListAgentsResult(AgentDirectoryPage);

impl ToolOutput for ListAgentsResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "list_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}

#[cfg(test)]
#[path = "list_agents_tests.rs"]
mod tests;
