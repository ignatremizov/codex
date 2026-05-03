use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub struct UnavailableToolHandler {
    tool_name: ToolName,
    spec: ToolSpec,
}

impl UnavailableToolHandler {
    pub fn with_spec(tool_name: ToolName) -> Self {
        let tool_display_name = flat_tool_name(&tool_name).into_owned();
        Self {
            tool_name,
            spec: ToolSpec::Function(ResponsesApiTool {
                name: tool_display_name.clone(),
                description: unavailable_tool_message(
                    tool_display_name,
                    "Calling it will return a placeholder error instead of executing.",
                ),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::new(),
                    Some(Vec::new()),
                    Some(false.into()),
                ),
                output_schema: None,
            }),
        }
    }
}

pub(crate) fn unavailable_tool_message(
    tool_name: impl std::fmt::Display,
    next_step: &str,
) -> String {
    format!(
        "Tool `{tool_name}` is not currently available. It appeared in earlier tool calls in this conversation, but its implementation is not available in the current request. {next_step}"
    )
}

impl ToolExecutor<ToolInvocation> for UnavailableToolHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl UnavailableToolHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation { payload, .. } = invocation;

        match payload {
            ToolPayload::Function { .. } => Ok(Box::new(FunctionToolOutput::from_text(
                unavailable_tool_message(
                    &self.tool_name,
                    "Retry after the tool becomes available or ask the user to re-enable it.",
                ),
                Some(false),
            ))),
            _ => Err(FunctionCallError::RespondToModel(
                "unavailable tool handler received unsupported payload".to_string(),
            )),
        }
    }
}

impl CoreToolRuntime for UnavailableToolHandler {}
