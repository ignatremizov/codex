//! Apply explicit resume choices through the target's ordered, durable settings API.

use super::LocalAgentControl;
use crate::agent::api::AgentInfo;
use crate::agent::api::ResumeRequest;
use crate::agent::child_config::apply_requested_agent_model_overrides;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadSettingsOverrides;

impl LocalAgentControl {
    pub(super) async fn resume_v2_agent(&self, request: ResumeRequest) -> CodexResult<AgentInfo> {
        let ResumeRequest {
            caller,
            target,
            resume_config,
            model,
            reasoning_effort,
        } = request;
        let target = self.resolve_target(caller, &target)?;
        if target == caller || target == ThreadId::from(self.session_id()) {
            return Err(CodexErr::UnsupportedOperation(
                "resume_agent must target another non-root agent".into(),
            ));
        }
        self.require_current_agent_ownership(target).await?;
        let _residency_pin = self
            .ensure_v2_agent_loaded(resume_config, target, /*parent*/ None)
            .await?;
        if model.is_some() || reasoning_effort.is_some() {
            self.update_resumed_agent_model(target, model.as_deref(), reasoning_effort)
                .await?;
        }
        self.inspect_agent(target).await
    }

    pub(crate) async fn update_resumed_agent_model(
        &self,
        thread_id: ThreadId,
        requested_model: Option<&str>,
        requested_reasoning_effort: Option<ReasoningEffort>,
    ) -> CodexResult<()> {
        let state = self.runtime.upgrade()?;
        let _lifecycle = state.v2_spawn_resume_lock(thread_id).lock_owned().await;
        state.ensure_membership_mutation_allowed(thread_id).await?;
        self.require_current_agent_ownership(thread_id).await?;
        let thread = state.get_thread(thread_id).await?;
        thread.ensure_not_unloading()?;
        let snapshot = thread.config_snapshot().await;
        // The original config supplies provider/catalog settings only. Live model choices
        // come from the target snapshot; no caller or subagent defaults replace them.
        let mut config = thread.config().await.as_ref().clone();
        config.model = Some(snapshot.model);
        config.model_reasoning_effort = snapshot.reasoning_effort;
        let effective_effort = requested_reasoning_effort
            .clone()
            .or_else(|| config.model_reasoning_effort.clone());
        apply_requested_agent_model_overrides(
            &thread.session,
            &mut config,
            requested_model,
            effective_effort,
            "resume_agent",
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
        thread
            .update_thread_settings(ThreadSettingsOverrides {
                model: requested_model.and(config.model),
                effort: requested_reasoning_effort.map(Some),
                ..Default::default()
            })
            .await
    }
}
