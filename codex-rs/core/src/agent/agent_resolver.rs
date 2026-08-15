use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use std::sync::Arc;

/// Resolves a single tool-facing agent target to a thread id.
pub(crate) async fn resolve_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    register_session_root(session, turn);
    if let Ok(thread_id) = ThreadId::from_string(target) {
        return Ok(thread_id);
    }

    session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, target)
        .await
        .map_err(|err| match err.details() {
            CodexErrorDetails::UnsupportedOperation(message) => {
                FunctionCallError::RespondToModel(message.clone())
            }
            _ => FunctionCallError::RespondToModel(err.to_string()),
        })
}

/// Resolve a V1 target that must already belong to the caller's root.
pub(crate) async fn resolve_controlled_v1_agent_target(
    session: &Arc<Session>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    session
        .services
        .agent_control
        .resolve_controlled_v1_agent_target(target)
        .await
        .map_err(agent_target_error)
}

/// Resolve a V1 resume target, allowing an explicit full UUID to enter the adoption path.
pub(crate) async fn resolve_resumable_v1_agent_target(
    session: &Arc<Session>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    session
        .services
        .agent_control
        .resolve_resumable_v1_agent_target(target)
        .await
        .map_err(agent_target_error)
}

fn agent_target_error(err: codex_protocol::error::CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErrorDetails::UnsupportedOperation(message) => {
            FunctionCallError::RespondToModel(message.clone())
        }
        _ => FunctionCallError::RespondToModel(err.to_string()),
    }
}

fn register_session_root(session: &Arc<Session>, turn: &Arc<TurnContext>) {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);
}
