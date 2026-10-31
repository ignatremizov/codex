//! Durable root-scoped agent alias projection.

use super::*;
use codex_app_server_protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

impl ThreadRequestProcessor {
    pub(super) async fn apply_current_agent_ownership(
        &self,
        thread: &mut Thread,
    ) -> Result<(), JSONRPCErrorError> {
        if !matches!(
            &thread.source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ) {
            return Ok(());
        }
        let Some(state_db) = &self.state_db else {
            return Ok(());
        };
        let thread_id = ThreadId::from_string(&thread.id)
            .map_err(|error| internal_error(format!("invalid stored thread id: {error}")))?;
        let Some(alias) = state_db
            .find_current_agent_alias_by_thread(thread_id)
            .await
            .map_err(|error| internal_error(format!("read current agent owner: {error}")))?
        else {
            return Ok(());
        };
        // Rollout source records provenance. Current durable edges also represent the
        // intentional absence of a parent after detachment, without rewriting history.
        thread.parent_thread_id = state_db
            .find_thread_spawn_parent(thread_id)
            .await
            .map_err(|error| internal_error(format!("read current agent parent: {error}")))?
            .map(|parent| parent.to_string());
        thread.session_id = alias.session_id.to_string();
        Ok(())
    }

    pub(super) async fn agent_alias_list_response_inner(
        &self,
        params: AgentAliasListParams,
    ) -> Result<AgentAliasListResponse, JSONRPCErrorError> {
        let AgentAliasListParams {
            root_thread_id,
            cursor,
            limit,
        } = params;
        let source_thread_id = ThreadId::from_string(&root_thread_id)
            .map_err(|err| invalid_request(format!("invalid root thread id: {err}")))?;
        let state_db = self
            .state_db
            .as_ref()
            .ok_or_else(|| internal_error("agent alias storage is unavailable"))?;
        let session_id = self
            .thread_manager
            .ensure_agent_alias_namespace_for_thread(source_thread_id)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to load the agent alias namespace for {source_thread_id}: {err}"
                ))
            })?;
        let aliases = state_db
            .list_agent_aliases(session_id)
            .await
            .map_err(|err| internal_error(format!("failed to list agent aliases: {err}")))?
            .into_iter()
            .map(|alias| AgentAlias {
                thread_id: alias.thread_id.to_string(),
                agent_ref: alias.agent_ref.to_string(),
                nickname: alias.nickname,
                task_path: alias.task_path,
                state: match alias.state {
                    codex_state::AgentAliasState::Active => AgentAliasState::Active,
                    codex_state::AgentAliasState::Closed => AgentAliasState::Closed,
                    codex_state::AgentAliasState::Transferred => AgentAliasState::Transferred,
                },
            })
            .collect::<Vec<_>>();
        let (data, next_cursor) = paginate_agent_aliases(&aliases, cursor, limit)?;
        Ok(AgentAliasListResponse { data, next_cursor })
    }
}

fn paginate_agent_aliases(
    aliases: &[AgentAlias],
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(Vec<AgentAlias>, Option<String>), JSONRPCErrorError> {
    let cursor = cursor
        .map(|cursor| {
            let cursor = cursor
                .parse::<u64>()
                .map_err(|err| invalid_request(format!("invalid agent alias cursor: {err}")))?;
            if cursor == 0 {
                return Err(invalid_request("agent alias cursors start at 1"));
            }
            Ok(cursor)
        })
        .transpose()?;
    let mut start = 0;
    if let Some(cursor) = cursor {
        for (index, alias) in aliases.iter().enumerate() {
            let agent_ref = alias.agent_ref.parse::<u64>().map_err(|err| {
                internal_error(format!("stored agent alias ref is invalid: {err}"))
            })?;
            if agent_ref > cursor {
                start = index;
                break;
            }
            start = aliases.len();
        }
    }
    let effective_limit = limit.unwrap_or(100).clamp(1, 100) as usize;
    let end = start.saturating_add(effective_limit).min(aliases.len());
    let data = aliases[start..end].to_vec();
    let next_cursor = data
        .last()
        .filter(|_| end < aliases.len())
        .map(|alias| alias.agent_ref.clone());
    Ok((data, next_cursor))
}
