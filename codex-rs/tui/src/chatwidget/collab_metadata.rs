//! Identity and explicitly supplied spawn settings shared by live and replayed agent labels.

use super::*;

impl ChatWidget {
    /// Stores or overwrites the cached nickname and role for a collab agent thread.
    ///
    /// Called by `App::upsert_agent_picker_thread` and `App::replace_chat_widget` to keep the
    /// rendering metadata in sync with the navigation cache. Must be called before any
    /// notification referencing this thread is processed, otherwise the rendered item will fall
    /// back to showing the raw thread id.
    pub(crate) fn set_collab_agent_metadata(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    ) {
        let metadata = self.collab_agent_metadata.entry(thread_id).or_default();
        metadata.agent_nickname = agent_nickname;
        metadata.agent_role = agent_role;
    }

    pub(crate) fn set_collab_agent_spawn_request(
        &mut self,
        thread_id: ThreadId,
        spawn_request: crate::multi_agents::SpawnRequestSummary,
    ) {
        self.collab_agent_metadata
            .entry(thread_id)
            .or_default()
            .spawn_request = Some(spawn_request);
    }

    /// An alias or successful lifecycle mapping can explicitly clear a task path.
    pub(crate) fn set_collab_agent_task_path(
        &mut self,
        thread_id: ThreadId,
        task_path: Option<String>,
    ) {
        self.collab_agent_metadata
            .entry(thread_id)
            .or_default()
            .task_path = crate::multi_agents::AgentTaskPath::Known(task_path);
    }

    pub(crate) fn set_collab_agent_ref(&mut self, thread_id: ThreadId, agent_ref: String) {
        self.collab_agent_metadata
            .entry(thread_id)
            .or_default()
            .agent_ref = Some(agent_ref);
    }

    /// Enrich replay/audit hints without clearing newer navigation metadata for missing fields.
    pub(crate) fn merge_collab_agent_metadata(
        &mut self,
        thread_id: ThreadId,
        metadata: AgentMetadata,
    ) {
        let existing = self.collab_agent_metadata.entry(thread_id).or_default();
        if metadata.agent_ref.is_some() {
            existing.agent_ref = metadata.agent_ref;
        }
        if metadata.agent_nickname.is_some() {
            existing.agent_nickname = metadata.agent_nickname;
        }
        if metadata.agent_role.is_some() {
            existing.agent_role = metadata.agent_role;
        }
        if metadata.spawn_request.is_some() {
            existing.spawn_request = metadata.spawn_request;
        }
        if matches!(
            &metadata.task_path,
            crate::multi_agents::AgentTaskPath::Known(_)
        ) {
            existing.task_path = metadata.task_path;
        }
    }

    pub(super) fn remember_user_agent_control_metadata(&mut self, item: &ThreadItem) {
        let ThreadItem::UserAgentControl {
            target_thread_id: Some(target_thread_id),
            nickname,
            role,
            agent_ref,
            task_path,
            task_path_mapping,
            status: codex_app_server_protocol::UserAgentControlStatus::Succeeded,
            ..
        } = item
        else {
            return;
        };
        let Some(thread_id) = crate::multi_agents::parse_thread_id(target_thread_id) else {
            return;
        };
        self.merge_collab_agent_metadata(
            thread_id,
            AgentMetadata {
                agent_ref: agent_ref.clone(),
                task_path: task_path
                    .as_ref()
                    .map(|path| crate::multi_agents::AgentTaskPath::Known(Some(path.clone())))
                    .unwrap_or_default(),
                agent_nickname: nickname.clone(),
                agent_role: role.clone(),
                spawn_request: crate::multi_agents::spawn_request_summary(item),
            },
        );
        for mapping in task_path_mapping {
            if let Some(thread_id) = crate::multi_agents::parse_thread_id(&mapping.thread_id) {
                self.set_collab_agent_task_path(thread_id, mapping.task_path.clone());
            }
        }
    }

    /// Registers the primary thread under the same stable label used by the agent picker.
    pub(crate) fn set_primary_collab_agent_metadata(&mut self, thread_id: ThreadId) {
        self.set_collab_agent_metadata(
            thread_id,
            Some("Main".to_string()),
            Some("default".to_string()),
        );
    }

    /// Returns the cached metadata for a thread, defaulting to empty if none has been registered.
    pub(super) fn collab_agent_metadata(&self, thread_id: ThreadId) -> AgentMetadata {
        self.collab_agent_metadata
            .get(&thread_id)
            .cloned()
            .unwrap_or_default()
    }
}
