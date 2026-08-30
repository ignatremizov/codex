use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;

use super::UserAgentForkMode;
use super::UserAgentSpawnOptions;
use super::UserAgentSpawnResult;
use super::child_session_source;
use super::user_control_tool_error;
use crate::CodexThread;
use crate::agent::control::ResponseObserverKind;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_role_and_model_overrides;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_service_tier;
use crate::tools::handlers::multi_agents_common::build_agent_spawn_config;

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
        let mut config =
            build_agent_spawn_config(&self.session.get_base_instructions().await, turn.as_ref())
                .map_err(user_control_tool_error)?;
        apply_spawn_agent_role_and_model_overrides(
            self.session.as_ref(),
            turn.as_ref(),
            &mut config,
            role.as_deref(),
            model.as_deref(),
            reasoning_effort,
        )
        .await
        .map_err(user_control_tool_error)?;
        apply_spawn_agent_service_tier(
            self.session.as_ref(),
            &mut config,
            turn.config.service_tier.as_deref(),
            /*requested_service_tier*/ None,
        )
        .await
        .map_err(user_control_tool_error)?;

        let spawn_id = uuid::Uuid::now_v7().as_simple().to_string();
        let task_name = (matches!(
            self.session.multi_agent_version(),
            Some(MultiAgentVersion::V2)
        ) || turn.config.multi_agent_version_from_features()
            == MultiAgentVersion::V2)
            .then(|| format!("user_{spawn_id}"));
        let session_source = child_session_source(self, turn.as_ref(), role.as_deref(), task_name)?;
        let fork_mode = match fork_mode {
            UserAgentForkMode::None => None,
            UserAgentForkMode::All => Some(SpawnAgentForkMode::FullHistory),
            UserAgentForkMode::LastNTurns(turns) => Some(SpawnAgentForkMode::LastNTurns(turns)),
        };
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
            environments: Some(turn.environments.to_selections()),
            multi_agent_v2_usage_hints: None,
            response_observation: response_handling.into(),
            response_observer: ResponseObserverKind::Durable,
        };
        let user_task_preview = input
            .as_ref()
            .filter(|_| response_handling.exposes_task_context())
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
        Ok(UserAgentSpawnResult {
            target_thread_id: agent.thread_id,
            agent_ref: agent.agent_ref,
            nickname: agent.metadata.agent_nickname,
            status: agent.status,
            post_admission_warning: agent.post_admission_warning,
        })
    }
}
