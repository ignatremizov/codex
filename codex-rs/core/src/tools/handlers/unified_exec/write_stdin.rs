use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::tools::sandboxing::ToolError;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UserInputWait;
use crate::unified_exec::WriteStdinInteractionEvent;
use crate::unified_exec::WriteStdinRequest;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::super::shell_spec::create_write_stdin_tool;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: i32,
    #[serde(default)]
    chars: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    wait_until_exit: bool,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub struct WriteStdinHandler {
    default_yield_time_ms: u64,
}

impl WriteStdinHandler {
    pub(crate) fn new(default_yield_time_ms: u64) -> Self {
        Self {
            default_yield_time_ms,
        }
    }
}

impl ToolExecutor<ToolInvocation> for WriteStdinHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write_stdin")
    }

    fn spec(&self) -> ToolSpec {
        create_write_stdin_tool(self.default_yield_time_ms)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl WriteStdinHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "write_stdin handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WriteStdinArgs = parse_arguments(&arguments)?;
        let context =
            UnifiedExecContext::new(session.clone(), step_context, cancellation_token, call_id);
        let yield_time_ms = args
            .yield_time_ms
            .unwrap_or(turn.unified_exec_write_stdin_yield_time_ms);
        let user_input_wait = if args.wait_until_exit {
            let turn_state = session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await;
            let (steer_activity_rx, pending_steer) = session
                .input_queue
                .subscribe_steer_activity(turn_state.as_deref())
                .await;
            Some(UserInputWait {
                steer_activity_rx,
                pending_steer,
            })
        } else {
            None
        };
        let response = match session
            .services
            .unified_exec_manager
            .write_stdin(
                &context,
                WriteStdinRequest {
                    process_id: args.session_id,
                    input: &args.chars,
                    yield_time_ms,
                    wait_until_exit: args.wait_until_exit,
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: turn.model_info().truncation_policy.into(),
                    interaction_event: Some(WriteStdinInteractionEvent {
                        session: &session,
                        turn: &turn,
                        interaction_id: &context.call_id,
                    }),
                    user_input_wait,
                },
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let message = match err {
                    UnifiedExecError::StdinApproval(ToolError::Rejected(reason)) => {
                        format!("write_stdin rejected: {reason}")
                    }
                    UnifiedExecError::StdinApproval(ToolError::Codex(err)) => {
                        format!("write_stdin approval failed: {err}")
                    }
                    err => format!("write_stdin failed: {err}"),
                };
                return Err(FunctionCallError::RespondToModel(message));
            }
        };

        Ok(boxed_tool_output(response))
    }
}

impl CoreToolRuntime for WriteStdinHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // `write_stdin` is transport for an existing exec session. Empty writes
        // are background polls, and non-empty writes continue a command that
        // already ran PreToolUse as Bash, so do not emit a second pre hook here.
        None
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // A `write_stdin` poll can observe final completion for the original
        // `exec_command`; emit that command's matching Bash PostToolUse.
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

#[cfg(test)]
#[path = "write_stdin_tests.rs"]
mod tests;
