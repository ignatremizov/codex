use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use super::AgentMetadata;
use super::LocalAgentControl;
use crate::config::Config;

pub(crate) struct ControlledResumeRegistration {
    reservation: crate::agent::registry::SpawnReservation,
    metadata: AgentMetadata,
}

impl LocalAgentControl {
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
            reservation,
            metadata,
        }))
    }
}

impl ControlledResumeRegistration {
    /// Called inside exact manager publication, after every fallible lifecycle check.
    pub(crate) fn commit(self) -> CodexResult<()> {
        if self.reservation.commit_if_absent(self.metadata) {
            Ok(())
        } else {
            Err(codex_protocol::error::CodexErr::InvalidRequest(
                "controlled resume registration changed during setup".into(),
            ))
        }
    }
}
