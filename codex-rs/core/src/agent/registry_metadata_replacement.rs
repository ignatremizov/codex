//! Exact-registration metadata replacement without releasing path or submission ownership.

use super::*;

pub(crate) struct AgentMetadataReplacement {
    state: Arc<AgentRegistry>,
    thread_id: ThreadId,
    metadata: AgentMetadata,
    previous: Option<RegisteredAgent>,
    new_key: String,
    reservation: Arc<()>,
    active: bool,
}

impl AgentRegistry {
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
        let previous = active_agents.thread_paths.get(&thread_id).cloned();
        if let Some(previous) = &previous
            && !active_agents
                .agent_tree
                .get(&previous.path)
                .is_some_and(|metadata| metadata.agent_id == Some(thread_id))
        {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {thread_id} has inconsistent registered metadata"
            )));
        }
        if previous.as_ref().map(|agent| &agent.path) != Some(&new_key)
            && active_agents.agent_tree.contains_key(&new_key)
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{new_key}` already exists"
            )));
        }
        for key in std::iter::once(&new_key).chain(previous.iter().map(|agent| &agent.path)) {
            if active_agents.metadata_reservations.contains_key(key) {
                return Err(CodexErr::UnsupportedOperation(format!(
                    "agent path `{key}` is reserved for restoration"
                )));
            }
        }
        let reservation = Arc::new(());
        for key in std::iter::once(&new_key).chain(previous.iter().map(|agent| &agent.path)) {
            active_agents
                .metadata_reservations
                .insert(key.clone(), Arc::clone(&reservation));
        }
        Ok(AgentMetadataReplacement {
            state: Arc::clone(self),
            thread_id,
            metadata,
            previous,
            new_key,
            reservation,
            active: true,
        })
    }
}

impl AgentMetadataReplacement {
    pub(crate) fn commit(mut self) -> Result<()> {
        let mut active_agents = self
            .state
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = active_agents.thread_paths.get(&self.thread_id).cloned();
        let registration_matches = match (&self.previous, &current) {
            (Some(previous), Some(current)) => {
                previous.path == current.path
                    && Arc::ptr_eq(&previous.submission, &current.submission)
            }
            (None, None) => true,
            (None, Some(_)) | (Some(_), None) => false,
        };
        let reservations_match = std::iter::once(&self.new_key)
            .chain(self.previous.iter().map(|agent| &agent.path))
            .all(|key| {
                active_agents
                    .metadata_reservations
                    .get(key)
                    .is_some_and(|reservation| Arc::ptr_eq(reservation, &self.reservation))
            });
        if !registration_matches || !reservations_match {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {} registration changed during restoration",
                self.thread_id
            )));
        }
        let current_metadata = current
            .as_ref()
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .filter(|metadata| metadata.agent_id == Some(self.thread_id))
            .cloned();
        if current.is_some() && current_metadata.is_none() {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {} metadata changed during restoration",
                self.thread_id
            )));
        }
        if active_agents
            .agent_tree
            .get(&self.new_key)
            .is_some_and(|metadata| metadata.agent_id != Some(self.thread_id))
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{}` already exists",
                self.new_key
            )));
        }
        let previous_was_counted = current_metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root));
        let replacement_is_counted = !self
            .metadata
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root);
        // Accepted assignments are live state, not canonical identity metadata. Preserve even
        // a concurrent clear rather than reinstalling the caller's older metadata snapshot.
        if let Some(current_metadata) = current_metadata {
            self.metadata.last_task_message = current_metadata.last_task_message;
        }
        if let Some(agent_nickname) = &self.metadata.agent_nickname {
            // Like ordinary registration, retain used names until the nickname pool resets.
            active_agents
                .used_agent_nicknames
                .insert(agent_nickname.clone());
        }
        let registration = if let Some(mut current) = current {
            if current.path != self.new_key {
                active_agents.agent_tree.remove(&current.path);
            }
            current.path = self.new_key.clone();
            current
        } else {
            let gate = active_agents.submission_gate(self.thread_id);
            RegisteredAgent::new(self.new_key.clone(), gate)
        };
        active_agents
            .thread_paths
            .insert(self.thread_id, registration);
        active_agents
            .agent_tree
            .insert(self.new_key.clone(), self.metadata.clone());
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
        self.release_reservations(&mut active_agents);
        self.active = false;
        Ok(())
    }

    fn release_reservations(&self, active_agents: &mut ActiveAgents) {
        for key in
            std::iter::once(&self.new_key).chain(self.previous.iter().map(|agent| &agent.path))
        {
            if active_agents
                .metadata_reservations
                .get(key)
                .is_some_and(|reservation| Arc::ptr_eq(reservation, &self.reservation))
            {
                active_agents.metadata_reservations.remove(key);
            }
        }
    }
}

impl Drop for AgentMetadataReplacement {
    fn drop(&mut self) {
        if self.active {
            let mut active_agents = self
                .state
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.release_reservations(&mut active_agents);
        }
    }
}

#[cfg(test)]
#[path = "registry_metadata_replacement_tests.rs"]
mod tests;
