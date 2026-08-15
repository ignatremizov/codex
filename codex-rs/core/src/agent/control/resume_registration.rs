use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use super::AgentControl;
use super::AgentMetadata;
use crate::config::Config;

pub(crate) struct ControlledResumeRegistration {
    control: AgentControl,
    reservation: crate::agent::registry::SpawnReservation,
    thread_id: ThreadId,
    metadata: AgentMetadata,
}

pub(crate) struct ControlledResumeRegistrationCommit {
    control: AgentControl,
    thread_id: ThreadId,
    published: bool,
}

impl AgentControl {
    pub(crate) async fn reserve_controlled_resume_registration(
        &self,
        config: &Config,
        thread_id: ThreadId,
        session_source: &SessionSource,
    ) -> CodexResult<Option<ControlledResumeRegistration>> {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_path,
            agent_nickname,
            agent_role,
            ..
        }) = session_source
        else {
            return Ok(None);
        };
        if self.state.agent_metadata_for_thread(thread_id).is_some() {
            return Ok(None);
        }
        if !config.ephemeral {
            self.sync_durable_agent_nickname_reservations().await?;
        }
        let mut reservation = self
            .state
            .reserve_spawn_slot(config.effective_agent_max_threads(MultiAgentVersion::V1))?;
        let mut metadata = self.prepare_restored_agent_metadata_exact(
            &mut reservation,
            agent_path.clone(),
            agent_role.clone(),
            agent_nickname.clone(),
        )?;
        metadata.agent_id = Some(thread_id);
        Ok(Some(ControlledResumeRegistration {
            control: self.clone(),
            reservation,
            thread_id,
            metadata,
        }))
    }
}

impl ControlledResumeRegistration {
    pub(crate) fn commit(self) -> ControlledResumeRegistrationCommit {
        self.reservation.commit(self.metadata);
        ControlledResumeRegistrationCommit {
            control: self.control,
            thread_id: self.thread_id,
            published: false,
        }
    }
}

impl ControlledResumeRegistrationCommit {
    pub(crate) fn publish(mut self) {
        self.published = true;
    }
}

impl Drop for ControlledResumeRegistrationCommit {
    fn drop(&mut self) {
        if !self.published {
            self.control.state.release_spawned_thread(self.thread_id);
        }
    }
}
