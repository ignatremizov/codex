use std::collections::BTreeMap;

use crate::compact::AutoCompactionDisabled;
use crate::function_tool::FunctionCallError;
use crate::session::inline_compaction;
use crate::state::TaskKind;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

pub(crate) struct CompactHandler {
    pub(crate) token_budget_mode: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactArgs {}

impl ToolExecutor<ToolInvocation> for CompactHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("compact")
    }

    fn spec(&self) -> ToolSpec {
        let description = if self.token_budget_mode {
            "Request a fresh context window for this thread and continue the current task automatically after the tool batch finishes. This resets context without summarizing. Save the useful findings, constraints, latest user request, and remaining work before calling. Files and running processes are preserved."
        } else {
            "Request this thread's /compact operation and continue the current task automatically after the tool batch finishes. Use sparingly when a task changes or extensive analysis, screenshots, or tool output can be summarized into the useful findings, constraints, latest user request, and remaining work. Only this thread's context changes; files and running processes are preserved."
        };
        ToolSpec::Function(ResponsesApiTool {
            name: "compact".to_string(),
            description: description.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "compact requires function arguments".to_string(),
                ));
            };
            let _: CompactArgs = parse_arguments(arguments)?;
            if invocation
                .session
                .services
                .thread_extension_data
                .get::<AutoCompactionDisabled>()
                .is_some()
                || invocation
                    .session
                    .services
                    .thread_extension_data
                    .get::<crate::codex_delegate::compaction::CompactionDecoder>()
                    .is_some()
            {
                return Err(FunctionCallError::RespondToModel(
                    "Compaction is disabled for this thread.".to_string(),
                ));
            }
            let active = invocation.session.active_turn.lock().await;
            if !active.as_ref().is_some_and(|active| {
                active.finishing.is_none()
                    && !active.terminal_pending
                    && active.task.as_ref().is_some_and(|task| {
                        task.kind == TaskKind::Regular
                            && task.turn_context.sub_id == invocation.turn.sub_id
                            && !task.cancellation_token.is_cancelled()
                    })
            }) {
                return Err(FunctionCallError::RespondToModel(
                    "Compaction requires this thread's active regular turn.".to_string(),
                ));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                "Compaction requested. After the current tool batch finishes, context will be compacted and this task will continue automatically.".to_string(),
                /*success*/ Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for CompactHandler {
    fn on_tool_result_accepted(&self, invocation: &ToolInvocation, _result: &dyn ToolOutput) {
        inline_compaction::request(&invocation.turn);
    }
}
