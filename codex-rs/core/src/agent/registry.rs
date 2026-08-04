use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use rand::prelude::IndexedRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Semaphore;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `AgentControl`
/// is).
#[derive(Default)]
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    /// Keeps mailbox enqueue order and `last_task_message` updates aligned per recipient.
    mailbox_submission_semaphores: Mutex<HashMap<ThreadId, Arc<Semaphore>>>,
    total_count: AtomicUsize,
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    thread_paths: HashMap<ThreadId, RegisteredAgent>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
}

#[derive(Clone)]
struct RegisteredAgent {
    path: String,
    evicted_environments: Option<Vec<TurnEnvironmentSelection>>,
}

impl RegisteredAgent {
    fn new(path: String) -> Self {
        Self {
            path,
            evicted_environments: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentMetadata {
    pub(crate) agent_id: Option<ThreadId>,
    pub(crate) agent_path: Option<AgentPath>,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) last_task_message: Option<String>,
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

impl AgentRegistry {
    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads,
                }));
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
        })
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let _ = self.release_spawned_thread_metadata_matching(thread_id, /*expected*/ None);
        self.remove_mailbox_submission_semaphore(thread_id);
    }

    pub(crate) fn release_spawned_thread_if_current(
        &self,
        thread_id: ThreadId,
        expected: &AgentMetadata,
    ) -> bool {
        if !self.release_spawned_thread_metadata_matching(thread_id, Some(expected)) {
            return false;
        }
        self.remove_mailbox_submission_semaphore(thread_id);
        true
    }

    fn release_spawned_thread_metadata_matching(
        &self,
        thread_id: ThreadId,
        expected: Option<&AgentMetadata>,
    ) -> bool {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(agent) = active_agents.thread_paths.get(&thread_id) else {
                return false;
            };
            let key = agent.path.clone();
            let current_metadata = active_agents.agent_tree.get(key.as_str());
            if expected.is_some_and(|expected| current_metadata != Some(expected)) {
                return false;
            }
            active_agents
                .thread_paths
                .remove(&thread_id)
                .and_then(|agent| active_agents.agent_tree.remove(agent.path.as_str()))
                .is_some_and(|metadata| {
                    !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
                })
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
        true
    }

    fn remove_mailbox_submission_semaphore(&self, thread_id: ThreadId) {
        self.mailbox_submission_semaphores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&thread_id);
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root_path = AgentPath::ROOT.to_string();
        let root_thread_id = active_agents
            .agent_tree
            .entry(root_path.clone())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            })
            .agent_id;
        if let Some(root_thread_id) = root_thread_id {
            active_agents
                .thread_paths
                .insert(root_thread_id, RegisteredAgent::new(root_path));
        }
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
    }

    pub(crate) fn save_evicted_environments(
        &self,
        thread_id: ThreadId,
        environments: Vec<TurnEnvironmentSelection>,
    ) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = Some(environments);
        }
    }

    pub(crate) fn evicted_environments(
        &self,
        thread_id: ThreadId,
    ) -> Option<Vec<TurnEnvironmentSelection>> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| agent.evicted_environments.clone())
    }

    pub(crate) fn clear_evicted_environments(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn reserve_agent_metadata_replacement(
        self: &Arc<Self>,
        thread_id: ThreadId,
        mut metadata: AgentMetadata,
    ) -> Result<AgentMetadataReplacement> {
        metadata.agent_id = Some(thread_id);
        let new_key = metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_key = active_agents
            .thread_paths
            .get(&thread_id)
            .map(|agent| agent.path.clone());
        let reserved_new_key = previous_key.as_ref() != Some(&new_key);
        if reserved_new_key {
            match active_agents.agent_tree.entry(new_key.clone()) {
                Entry::Occupied(_) => {
                    return Err(CodexErr::UnsupportedOperation(format!(
                        "agent path `{new_key}` already exists"
                    )));
                }
                Entry::Vacant(entry) => {
                    entry.insert(AgentMetadata {
                        agent_path: metadata.agent_path.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        drop(active_agents);
        Ok(AgentMetadataReplacement {
            state: Arc::clone(self),
            thread_id,
            metadata,
            new_key,
            reserved_new_key,
            active: true,
        })
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn mailbox_submission_semaphore(&self, thread_id: ThreadId) -> Arc<Semaphore> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active_agents
            .agent_tree
            .values()
            .any(|metadata| metadata.agent_id == Some(thread_id))
        {
            return Arc::new(Semaphore::new(1));
        }
        Arc::clone(
            self.mailbox_submission_semaphores
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(thread_id)
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        )
    }

    pub(crate) fn update_last_task_message(&self, thread_id: ThreadId, last_task_message: String) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metadata) = active_agents
            .agent_tree
            .values_mut()
            .find(|metadata| metadata.agent_id == Some(thread_id))
        {
            metadata.last_task_message = Some(last_task_message);
        }
    }

    pub(crate) fn clear_last_task_message(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metadata) = active_agents
            .agent_tree
            .values_mut()
            .find(|metadata| metadata.agent_id == Some(thread_id))
        {
            metadata.last_task_message = None;
        }
    }

    fn register_spawned_thread(&self, agent_metadata: AgentMetadata) {
        let Some(thread_id) = agent_metadata.agent_id else {
            return;
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        if let Some(previous_agent) = active_agents
            .thread_paths
            .insert(thread_id, RegisteredAgent::new(key.clone()))
            && previous_agent.path != key
        {
            active_agents
                .agent_tree
                .remove(previous_agent.path.as_str());
        }
        if let Some(previous_metadata) = active_agents.agent_tree.insert(key, agent_metadata)
            && let Some(previous_thread_id) = previous_metadata.agent_id
            && previous_thread_id != thread_id
        {
            active_agents.thread_paths.remove(&previous_thread_id);
        }
    }

    fn register_spawned_thread_if_absent(&self, agent_metadata: AgentMetadata) -> bool {
        let Some(thread_id) = agent_metadata.agent_id else {
            return false;
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents.thread_paths.contains_key(&thread_id) {
            return false;
        }
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if active_agents
            .agent_tree
            .get(&key)
            .is_some_and(|metadata| metadata.agent_id.is_some())
        {
            return false;
        }
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        active_agents
            .thread_paths
            .insert(thread_id, RegisteredAgent::new(key.clone()));
        active_agents.agent_tree.insert(key, agent_metadata);
        true
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_names: Vec<String> = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .collect();
            if let Some(name) = available_names.choose(&mut rand::rng()) {
                name.clone()
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
}

pub(crate) struct AgentMetadataReplacement {
    state: Arc<AgentRegistry>,
    thread_id: ThreadId,
    metadata: AgentMetadata,
    new_key: String,
    reserved_new_key: bool,
    active: bool,
}

impl AgentMetadataReplacement {
    pub(crate) fn commit(mut self) -> Result<()> {
        self.commit_inner(/*expected*/ None).map(drop)
    }

    pub(crate) fn commit_if_current(mut self, expected: &AgentMetadata) -> Result<bool> {
        self.commit_inner(Some(expected))
    }

    fn commit_inner(&mut self, expected: Option<&AgentMetadata>) -> Result<bool> {
        let mut active_agents = self
            .state
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(&self.new_key)
            .is_some_and(|metadata| {
                metadata
                    .agent_id
                    .is_some_and(|agent_id| agent_id != self.thread_id)
            })
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{}` already exists",
                self.new_key
            )));
        }
        let current_registration = active_agents.thread_paths.get(&self.thread_id).cloned();
        let current_key = current_registration
            .as_ref()
            .map(|registration| registration.path.clone());
        let current_metadata = current_key
            .as_ref()
            .and_then(|current_key| active_agents.agent_tree.get(current_key.as_str()))
            .filter(|metadata| metadata.agent_id == Some(self.thread_id))
            .cloned();
        if expected.is_some_and(|expected| current_metadata.as_ref() != Some(expected)) {
            return Ok(false);
        }
        if let Some(current_nickname) = current_metadata
            .as_ref()
            .and_then(|metadata| metadata.agent_nickname.as_ref())
        {
            active_agents.used_agent_nicknames.remove(current_nickname);
        }
        if let Some(agent_nickname) = self.metadata.agent_nickname.as_ref() {
            active_agents
                .used_agent_nicknames
                .insert(agent_nickname.clone());
        }
        if let Some(current_key) = current_key.as_ref()
            && current_key != &self.new_key
            && current_metadata.is_some()
        {
            active_agents.agent_tree.remove(current_key);
        }
        active_agents.thread_paths.insert(
            self.thread_id,
            RegisteredAgent {
                path: self.new_key.clone(),
                evicted_environments: current_registration
                    .and_then(|registration| registration.evicted_environments),
            },
        );
        active_agents
            .agent_tree
            .insert(self.new_key.clone(), self.metadata.clone());
        let previous_was_counted = current_metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root));
        let replacement_is_counted = !self
            .metadata
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root);
        match (previous_was_counted, replacement_is_counted) {
            (false, true) => {
                self.state
                    .total_count
                    .fetch_add(/*val*/ 1, Ordering::AcqRel);
            }
            (true, false) => {
                self.state
                    .total_count
                    .fetch_sub(/*val*/ 1, Ordering::AcqRel);
            }
            (false, false) | (true, true) => {}
        }
        self.active = false;
        Ok(true)
    }
}

impl Drop for AgentMetadataReplacement {
    fn drop(&mut self) {
        if !self.active || !self.reserved_new_key {
            return;
        }
        let mut active_agents = self
            .state
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(&self.new_key)
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(&self.new_key);
        }
    }
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
            })?;
        self.reserved_agent_nickname = Some(agent_nickname.clone());
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) {
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.state.register_spawned_thread(agent_metadata);
        self.active = false;
    }

    pub(crate) fn commit_if_absent(mut self, agent_metadata: AgentMetadata) -> bool {
        if !self.state.register_spawned_thread_if_absent(agent_metadata) {
            return false;
        }
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.active = false;
        true
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
