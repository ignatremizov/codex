use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::UserAgentForkMode;
use super::UserAgentSpawnOptions;
use super::UserAgentSpawnResult;
use super::child_session_source;
use crate::CodexThread;
use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigOrigin;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;

impl CodexThread {
    /// Spawn a default or configured-role child, optionally starting its first turn.
    pub async fn spawn_agent(
        &self,
        options: UserAgentSpawnOptions,
    ) -> CodexResult<UserAgentSpawnResult> {
        let UserAgentSpawnOptions {
            role,
            model,
            reasoning_effort,
            input,
            fork_mode,
            response_handling,
        } = options;
        if input.as_ref().is_some_and(Vec::is_empty) {
            return Err(CodexErr::InvalidRequest(
                "agent prompt requires nonempty user input".to_string(),
            ));
        }
        if matches!(fork_mode, UserAgentForkMode::LastNTurns(0)) {
            return Err(CodexErr::InvalidRequest(
                "last-N agent forks require a positive turn count".to_string(),
            ));
        }

        let turn = self.session.new_default_turn().await;
        let step_context = self
            .session
            .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
            .await?;
        let fork_mode = match fork_mode {
            UserAgentForkMode::None => None,
            UserAgentForkMode::All => Some(SpawnAgentForkMode::FullHistory),
            UserAgentForkMode::LastNTurns(turns) => Some(SpawnAgentForkMode::LastNTurns(turns)),
        };
        let prepared = prepare_agent_spawn_config(
            self.session.as_ref(),
            &step_context,
            SpawnConfigOptions {
                origin: SpawnConfigOrigin::User,
                version: match turn.multi_agent_version {
                    MultiAgentVersion::V2 => SpawnConfigVersion::V2,
                    MultiAgentVersion::V1 | MultiAgentVersion::Disabled => SpawnConfigVersion::V1,
                },
                fork_mode: fork_mode.as_ref(),
                role_name: role.as_deref(),
                model: model.as_deref(),
                service_tier: None,
                reasoning_effort,
            },
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
        let config = prepared.config;

        let spawn_id = uuid::Uuid::now_v7().as_simple().to_string();
        let task_name = (matches!(
            self.session.multi_agent_version(),
            Some(MultiAgentVersion::V2)
        ) || turn.config.multi_agent_version_from_features()
            == MultiAgentVersion::V2)
            .then(|| format!("user_{spawn_id}"));
        let session_source = child_session_source(
            self,
            turn.as_ref(),
            prepared.role_name.as_deref(),
            task_name,
        )?;
        let fork_parent_spawn_call_id = fork_mode
            .as_ref()
            .map(|_| format!("user-agent-spawn-{spawn_id}"));
        let options = SpawnAgentOptions {
            fork_parent_spawn_call_id,
            fork_mode,
            parent_thread_id: Some(self.session.thread_id()),
            parent_turn_id: None,
            root_turn_id: None,
            cyber_access_program: turn.cyber_access_program,
            environments: Some(step_context.environments.to_selections()),
            response_observation: response_handling.into(),
            ..Default::default()
        };
        let user_task_preview = input
            .as_ref()
            .map(|input| crate::agent::control::render_input_preview(input));
        let agent = match input {
            Some(input) => {
                self.session
                    .services
                    .agent_control
                    .spawn_user_agent_with_metadata(
                        config,
                        input,
                        user_task_preview,
                        Some(session_source),
                        options,
                    )
                    .await?
            }
            None => {
                self.session
                    .services
                    .agent_control
                    .spawn_idle_agent_with_metadata(config, Some(session_source), options)
                    .await?
            }
        };
        Ok(agent)
    }
}
