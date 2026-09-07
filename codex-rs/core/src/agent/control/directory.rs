//! Read-only, user-enabled discovery of the current root's durable agent aliases.

use super::*;
use serde::Deserialize;

/// Selection applied before pagination; omission selects only loaded threads.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentDirectoryStatus {
    #[default]
    Loaded,
    Running,
    PendingInit,
    Completed,
    Interrupted,
    Errored,
    Closed,
    All,
}

/// Lifecycle summary without the target's potentially large or private final response.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentDirectoryEntryStatus {
    PendingInit,
    Running,
    Completed,
    Interrupted,
    Errored,
    Closed,
    Unloaded,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AgentDirectoryEntry {
    pub(crate) agent_id: ThreadId,
    #[serde(rename = "ref")]
    pub(crate) agent_ref: String,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) task_path: Option<String>,
    pub(crate) status: AgentDirectoryEntryStatus,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AgentDirectoryPage {
    pub(crate) agents: Vec<AgentDirectoryEntry>,
    pub(crate) next_cursor: Option<String>,
}

impl AgentControl {
    /// The owning root's current user configuration is authoritative, not a child's role.
    pub(crate) async fn agent_directory_enabled(
        &self,
        caller_thread_id: ThreadId,
    ) -> CodexResult<bool> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(false);
        };
        let state = self.upgrade()?;
        let root = ThreadId::from(session_id);
        let Ok(thread) = state.get_thread_including_pending(root).await else {
            return Ok(false);
        };
        self.require_current_agent_ownership(caller_thread_id)
            .await?;
        Ok(thread.session.get_config().await.list_agents_enabled)
    }

    pub(crate) async fn list_agent_directory(
        &self,
        caller_thread_id: ThreadId,
        path_prefix: Option<&str>,
        filter: AgentDirectoryStatus,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> CodexResult<AgentDirectoryPage> {
        if !self.agent_directory_enabled(caller_thread_id).await? {
            return Err(CodexErr::InvalidRequest(
                "V1 list_agents is disabled; enable tools.list_agents.enabled in user configuration"
                    .to_string(),
            ));
        }
        let after = cursor
            .map(str::parse::<u64>)
            .transpose()
            .map_err(|_| CodexErr::InvalidRequest("invalid agent directory cursor".to_string()))?
            .unwrap_or_default();
        let limit = limit.unwrap_or(50).min(200) as usize;
        if limit == 0 {
            return Err(CodexErr::InvalidRequest(
                "agent directory limit must be positive".to_string(),
            ));
        }
        let prefix = match path_prefix {
            Some(prefix) => Some(
                self.resolve_agent_task_path(caller_thread_id, prefix)
                    .await?,
            ),
            None => None,
        };
        let mut aliases = self.list_session_agent_aliases().await?;
        aliases.sort_by_key(|alias| alias.agent_ref);
        let state = self.upgrade()?;
        let mut agents = Vec::new();
        let mut last_ref = None;
        let mut next_cursor = None;
        for alias in aliases {
            if alias.state == codex_agent_graph_store::AgentAliasState::Transferred
                || alias.agent_ref <= after
                || prefix.as_ref().is_some_and(|prefix| {
                    alias.task_path.as_ref().is_none_or(|path| {
                        path != prefix
                            && !path
                                .strip_prefix(prefix.as_str())
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
                })
            {
                continue;
            }
            // Setup-pending runtimes are not published members available for discovery.
            let thread = state.get_thread(alias.thread_id).await.ok();
            let status = if alias.state == codex_agent_graph_store::AgentAliasState::Closed {
                AgentDirectoryEntryStatus::Closed
            } else if let Some(thread) = &thread {
                match thread.agent_status().await {
                    AgentStatus::PendingInit => AgentDirectoryEntryStatus::PendingInit,
                    AgentStatus::Running => AgentDirectoryEntryStatus::Running,
                    AgentStatus::Completed(_) => AgentDirectoryEntryStatus::Completed,
                    AgentStatus::Interrupted => AgentDirectoryEntryStatus::Interrupted,
                    AgentStatus::Errored(_) => AgentDirectoryEntryStatus::Errored,
                    AgentStatus::Shutdown => AgentDirectoryEntryStatus::Closed,
                    AgentStatus::NotFound => AgentDirectoryEntryStatus::Unloaded,
                }
            } else {
                AgentDirectoryEntryStatus::Unloaded
            };
            let selected = match filter {
                AgentDirectoryStatus::Loaded => {
                    thread.is_some() && status != AgentDirectoryEntryStatus::Closed
                }
                AgentDirectoryStatus::Running => status == AgentDirectoryEntryStatus::Running,
                AgentDirectoryStatus::PendingInit => {
                    status == AgentDirectoryEntryStatus::PendingInit
                }
                AgentDirectoryStatus::Completed => status == AgentDirectoryEntryStatus::Completed,
                AgentDirectoryStatus::Interrupted => {
                    status == AgentDirectoryEntryStatus::Interrupted
                }
                AgentDirectoryStatus::Errored => status == AgentDirectoryEntryStatus::Errored,
                AgentDirectoryStatus::Closed => status == AgentDirectoryEntryStatus::Closed,
                AgentDirectoryStatus::All => true,
            };
            if !selected {
                continue;
            }
            if agents.len() == limit {
                next_cursor = last_ref;
                break;
            }
            last_ref = Some(alias.agent_ref.to_string());
            agents.push(AgentDirectoryEntry {
                agent_id: alias.thread_id,
                agent_ref: alias.agent_ref.to_string(),
                nickname: alias.nickname,
                role: self
                    .get_agent_metadata(alias.thread_id)
                    .and_then(|metadata| metadata.agent_role),
                task_path: alias.task_path,
                status,
            });
        }
        Ok(AgentDirectoryPage {
            agents,
            next_cursor,
        })
    }
}
