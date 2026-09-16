//! Explicit user spawning shares setup and limits, but preserves already-admitted input.

use super::spawn::SpawnInitialInput;
use super::*;
use crate::agent::UserAgentSpawnResult;
use crate::agent::types::SpawnAgentOptions;

impl LocalAgentControl {
    pub(crate) async fn spawn_user_agent_with_metadata(
        &self,
        config: Config,
        input: Vec<UserInput>,
        task_preview: Option<String>,
        source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<UserAgentSpawnResult> {
        self.spawn_user_agent_owned(config, Some(input), task_preview, source, options)
            .await
    }

    pub(crate) async fn spawn_idle_agent_with_metadata(
        &self,
        config: Config,
        source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<UserAgentSpawnResult> {
        self.spawn_user_agent_owned(
            config, /*input*/ None, /*task_preview*/ None, source, options,
        )
        .await
    }

    async fn spawn_user_agent_owned(
        &self,
        config: Config,
        input: Option<Vec<UserInput>>,
        task_preview: Option<String>,
        source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<UserAgentSpawnResult> {
        let control = self.clone();
        let spawned = if input.is_none() {
            self.spawn_with_receipt(
                config,
                SpawnInitialInput::UserControlled {
                    input,
                    task_preview,
                },
                source,
                options,
            )
            .await?
        } else {
            tokio::spawn(async move {
                Box::pin(control.spawn_agent_owned(
                    config,
                    SpawnInitialInput::UserControlled {
                        input,
                        task_preview,
                    },
                    source,
                    options,
                ))
                .await
            })
            .await
            .map_err(|error| CodexErr::Fatal(format!("user spawn worker failed: {error}")))??
        };
        Ok(UserAgentSpawnResult {
            target_thread_id: spawned.agent.thread_id,
            agent_ref: spawned.alias.as_ref().map(|alias| alias.agent_ref),
            task_path: spawned
                .alias
                .as_ref()
                .and_then(|alias| alias.task_path.clone()),
            nickname: spawned
                .alias
                .and_then(|alias| alias.nickname)
                .or(spawned.agent.metadata.agent_nickname),
            status: spawned.agent.status,
            post_admission_warning: spawned.post_admission_warning,
            input_outcome: spawned.input_outcome,
        })
    }
}
