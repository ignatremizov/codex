//! Process-lifetime queued agent-turn projection and cancellation.

use super::agent_control::conversion::agent_response_handling;
use super::*;

impl ThreadRequestProcessor {
    pub(super) async fn agent_queue_list_response_inner(
        &self,
        params: AgentQueueListParams,
    ) -> Result<AgentQueueListResponse, JSONRPCErrorError> {
        let AgentQueueListParams {
            root_thread_id,
            cursor,
            limit,
        } = params;
        let root_thread_id = parse_agent_queue_root(&root_thread_id)?;
        let thread = self
            .thread_manager
            .get_thread(root_thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {root_thread_id}")))?;
        let queued = thread
            .list_user_agent_queued_turns()
            .into_iter()
            .map(agent_queue_entry)
            .collect::<Vec<_>>();
        let (data, next_cursor) = paginate_agent_queue(&queued, cursor, limit)?;
        Ok(AgentQueueListResponse { data, next_cursor })
    }

    pub(super) async fn agent_queue_delete_response_inner(
        &self,
        params: AgentQueueDeleteParams,
    ) -> Result<AgentQueueDeleteResponse, JSONRPCErrorError> {
        let AgentQueueDeleteParams { root_thread_id, id } = params;
        let root_thread_id = parse_agent_queue_root(&root_thread_id)?;
        let thread = self
            .thread_manager
            .get_thread(root_thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {root_thread_id}")))?;
        let removed = thread
            .cancel_user_agent_queued_turn(&id)
            .map_err(agent_control::conversion::agent_control_error)?;
        if !removed {
            return Err(invalid_request(format!(
                "agent queue entry not found: {id}"
            )));
        }
        Ok(AgentQueueDeleteResponse { id })
    }
}

fn parse_agent_queue_root(value: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value)
        .map_err(|err| invalid_request(format!("invalid root thread id: {err}")))
}

fn agent_queue_entry(turn: UserAgentQueuedTurn) -> AgentQueueEntry {
    AgentQueueEntry {
        id: turn.id,
        source_thread_id: turn.source_thread_id.to_string(),
        target_thread_id: turn.target_thread_id.to_string(),
        input: turn.input.into_iter().map(V2UserInput::from).collect(),
        prompt_preview: turn.prompt_preview,
        response_handling: agent_response_handling(turn.response_handling),
        authored_selector: turn.authored_selector,
    }
}

fn paginate_agent_queue(
    queued: &[AgentQueueEntry],
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(Vec<AgentQueueEntry>, Option<String>), JSONRPCErrorError> {
    let cursor = cursor
        .map(|cursor| {
            Uuid::parse_str(&cursor)
                .map_err(|err| invalid_request(format!("invalid agent queue cursor: {err}")))
        })
        .transpose()?;
    let start = match cursor {
        Some(cursor) => queued
            .iter()
            .position(|entry| Uuid::parse_str(&entry.id).is_ok_and(|entry_id| entry_id > cursor))
            .unwrap_or(queued.len()),
        None => 0,
    };
    let effective_limit = limit.unwrap_or(100).clamp(1, 100) as usize;
    let end = start.saturating_add(effective_limit).min(queued.len());
    let data = queued[start..end].to_vec();
    let next_cursor = data
        .last()
        .filter(|_| end < queued.len())
        .map(|entry| entry.id.clone());
    Ok((data, next_cursor))
}

#[cfg(test)]
#[path = "agent_queue_tests.rs"]
mod tests;
