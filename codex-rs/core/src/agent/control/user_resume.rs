//! Controlled restoration and explicitly authorized ownership transfer.

use super::aliases::PersistedAgentSpawn;
use super::aliases::ThreadSpawnPersistence;
use super::*;
use crate::CodexThread;
use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentAliasTransfer;

pub(super) enum ResumeAuthority {
    Recorded,
    ModelControlled,
    Controlled,
    Transfer {
        previous_session_id: Option<SessionId>,
        descendants: Vec<ThreadId>,
        authored_selector: String,
    },
}

impl ResumeAuthority {
    pub(super) fn persistence(self) -> ThreadSpawnPersistence {
        match self {
            Self::Recorded => ThreadSpawnPersistence::Resume,
            Self::ModelControlled | Self::Controlled => ThreadSpawnPersistence::ControlledResume,
            Self::Transfer {
                previous_session_id,
                descendants,
                authored_selector,
            } => ThreadSpawnPersistence::Transfer {
                expected_previous_session_id: previous_session_id,
                reserved_descendant_thread_ids: Some(descendants),
                authored_selector,
            },
        }
    }
}

pub(super) struct RestoredAgent {
    pub(super) thread: Arc<CodexThread>,
    pub(super) version: MultiAgentVersion,
    pub(super) persisted: PersistedAgentSpawn,
    pub(super) post_commit_warning: Option<String>,
}

pub(crate) struct UserResumeOutcome {
    pub(crate) alias: Option<AgentAlias>,
    pub(crate) transfer: Option<AgentAliasTransfer>,
    pub(crate) post_commit_warning: Option<String>,
}

impl LocalAgentControl {
    /// Model-authored transfer retains the autonomous new-edge depth budget.
    pub(crate) async fn resume_agent_from_rollout_adopting(
        &self,
        config: Config,
        thread_id: ThreadId,
        source: SessionSource,
        policy: ResponseObservationPolicy,
        previous_session_id: Option<SessionId>,
        authored_selector: String,
    ) -> CodexResult<ThreadId> {
        if thread_spawn_depth(&source).is_some_and(|depth| depth > config.agent_max_depth) {
            return Err(CodexErr::InvalidRequest(
                "agent adoption exceeds the model delegation depth limit".into(),
            ));
        }
        if ThreadId::from_string(
            authored_selector
                .strip_prefix("id:")
                .unwrap_or(&authored_selector),
        )
        .ok()
            != Some(thread_id)
        {
            return Err(CodexErr::InvalidRequest(
                "adoption requires the target's canonical UUID".into(),
            ));
        }
        let result = self
            .resume_with_observation(
                config,
                thread_id,
                source,
                policy,
                ResumeAuthority::Transfer {
                    previous_session_id,
                    descendants: Vec::new(),
                    authored_selector,
                },
            )
            .await?;
        if let Some(warning) = result.post_commit_warning {
            return Err(CodexErr::Fatal(warning));
        }
        Ok(thread_id)
    }
    pub(crate) async fn resume_user_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        observer_source: SessionSource,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<UserResumeOutcome> {
        self.resume_with_observation(
            config,
            thread_id,
            observer_source,
            policy,
            ResumeAuthority::Controlled,
        )
        .await
    }

    pub(crate) async fn resume_user_agent_from_rollout_adopting(
        &self,
        config: Config,
        thread_id: ThreadId,
        observer_source: SessionSource,
        policy: ResponseObservationPolicy,
        previous_session_id: Option<SessionId>,
        authored_selector: String,
    ) -> CodexResult<UserResumeOutcome> {
        if ThreadId::from_string(
            authored_selector
                .strip_prefix("id:")
                .unwrap_or(&authored_selector),
        )
        .ok()
            != Some(thread_id)
        {
            return Err(CodexErr::InvalidRequest(
                "adoption requires the target's canonical UUID".into(),
            ));
        }
        self.resume_with_observation(
            config,
            thread_id,
            observer_source,
            policy,
            ResumeAuthority::Transfer {
                previous_session_id,
                descendants: Vec::new(),
                authored_selector,
            },
        )
        .await
    }

    async fn resume_with_observation(
        &self,
        config: Config,
        thread_id: ThreadId,
        observer_source: SessionSource,
        policy: ResponseObservationPolicy,
        mut authority: ResumeAuthority,
    ) -> CodexResult<UserResumeOutcome> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let guard = state.agent_lifecycle_lock(thread_id).lock_owned().await;
            state.check_restoration_fence(thread_id)?;
            if state.get_thread(thread_id).await.is_ok() {
                return Err(CodexErr::InvalidRequest(
                    "target became live before resume".into(),
                ));
            }
            let mut descendant_guards = Vec::new();
            let mut writers = None;
            let source = match &mut authority {
                ResumeAuthority::Recorded
                | ResumeAuthority::ModelControlled
                | ResumeAuthority::Controlled => {
                    control.require_current_agent_ownership(thread_id).await?;
                    control
                        .canonical_controlled_resume_source(thread_id, observer_source.clone())
                        .await?
                }
                ResumeAuthority::Transfer { descendants, .. } => {
                    let graph = state
                        .agent_graph_store()
                        .filter(|graph| graph.supports_agent_aliases())
                        .ok_or_else(|| {
                            CodexErr::UnsupportedOperation(
                                "adoption requires durable aliases".into(),
                            )
                        })?;
                    *descendants = graph
                        .list_thread_spawn_descendants(thread_id, /*status_filter*/ None)
                        .await
                        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                    descendants.sort_by_key(ToString::to_string);
                    descendants.dedup();
                    if descendants.contains(&thread_id)
                        || observer_source
                            .parent_thread_id()
                            .is_some_and(|id| id == thread_id || descendants.contains(&id))
                    {
                        return Err(CodexErr::InvalidRequest(
                            "cannot adopt a thread beneath its own subtree".into(),
                        ));
                    }
                    for descendant in descendants.iter().copied() {
                        descendant_guards.push(
                            state
                                .agent_lifecycle_lock(descendant)
                                .try_lock_owned()
                                .map_err(|_| {
                                    CodexErr::InvalidRequest("adoption subtree is busy".into())
                                })?,
                        );
                        if state.get_thread(descendant).await.is_ok() {
                            return Err(CodexErr::InvalidRequest(format!(
                                "close live descendant {descendant} before adoption"
                            )));
                        }
                    }
                    writers = Some(state.reserve_thread_writers(descendants.clone()).await?);
                    observer_source.clone()
                }
            };
            let root_depth = thread_spawn_depth(&source).unwrap_or(0);
            // Target startup acquires its writer. Descendant reservations stay held through
            // the alias transaction's exact-membership CAS and runtime publication.
            let restored = control
                .resume_single_agent_from_rollout_unlocked(
                    config.clone(),
                    thread_id,
                    source,
                    /*initial_history_override*/ None,
                    /*client_mcp_extensions_override*/ None,
                    authority,
                )
                .await?;
            drop(writers);
            drop(descendant_guards);
            drop(guard);
            let mut warning = restored.post_commit_warning;
            if warning.is_none()
                && let Err(error) = control
                    .restore_open_agent_descendants(
                        &config,
                        thread_id,
                        root_depth,
                        restored.version,
                    )
                    .await
            {
                warning = Some(format!(
                    "agent resumed, but descendant restoration failed: {error}"
                ));
            }
            if warning.is_none() && policy.final_response() != FinalResponseObservation::None {
                let observed = restored.thread.agent_status().await;
                if let Err(error) = control
                    .ensure_durable_completion_watcher(
                        thread_id,
                        observer_source,
                        policy,
                        observed,
                        /*task_preview*/ None,
                    )
                    .await
                {
                    if matches!(
                        restored.persisted.transfer,
                        Some(AgentAliasTransfer::Transferred { .. })
                    ) {
                        warning = Some(format!(
                            "ownership transferred, but response observation failed: {error}"
                        ));
                    } else {
                        return Err(error);
                    }
                }
            }
            Ok(UserResumeOutcome {
                alias: restored.persisted.alias,
                transfer: restored.persisted.transfer,
                post_commit_warning: warning,
            })
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user resume worker failed: {error}")))?
    }

    pub(crate) async fn resume_agent_from_rollout_with_user_input(
        &self,
        config: Config,
        thread_id: ThreadId,
        source: SessionSource,
        admission: ResumeUserInputAdmission,
    ) -> CodexResult<ResponseObservationSubmission> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let guard = state.agent_lifecycle_lock(thread_id).lock_owned().await;
            control.require_current_agent_ownership(thread_id).await?;
            let source = control
                .canonical_controlled_resume_source(thread_id, source)
                .await?;
            let root_depth = thread_spawn_depth(&source).unwrap_or(0);
            let restored = control
                .resume_single_agent_from_rollout_unlocked(
                    config.clone(),
                    thread_id,
                    source,
                    /*initial_history_override*/ None,
                    /*client_mcp_extensions_override*/ None,
                    ResumeAuthority::Controlled,
                )
                .await?;
            // No NextTurn policy was installed: this one exact admission owns its cursor.
            let mut submission = control
                .submit_user_input_to_thread_locked(&restored.thread, admission)
                .await?;
            drop(guard);
            if let Err(error) = control
                .restore_open_agent_descendants(&config, thread_id, root_depth, restored.version)
                .await
            {
                let warning = format!(
                    "input was enqueued; descendant restoration failed: {error}; do not resend"
                );
                submission.post_admission_warning = Some(match submission.post_admission_warning {
                    Some(existing) => format!("{existing}; {warning}"),
                    None => warning,
                });
            }
            Ok(submission)
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user resume/input worker failed: {error}")))?
    }

    /// Preserve durable topology; the supplied source denotes an observer, never a new parent.
    pub(crate) async fn canonical_controlled_resume_source(
        &self,
        thread_id: ThreadId,
        fallback: SessionSource,
    ) -> CodexResult<SessionSource> {
        let state = self.upgrade()?;
        let stored = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        if thread_id == ThreadId::from(self.session_id()) {
            return Ok(stored.source);
        }
        let Some(graph) = state.agent_graph_store() else {
            return Ok(stored.source);
        };
        let parent_id = graph
            .find_thread_spawn_parent(thread_id)
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
            .or(stored.parent_thread_id)
            .or_else(|| stored.source.parent_thread_id())
            .ok_or_else(|| {
                CodexErr::InvalidRequest("controlled child has no recorded parent".into())
            })?;
        let mut ancestor = parent_id;
        let mut seen = HashSet::from([thread_id]);
        let mut depth = 1i32;
        while ancestor != ThreadId::from(self.session_id()) {
            if !seen.insert(ancestor) {
                return Err(CodexErr::InvalidRequest(
                    "cyclic controlled ancestry".into(),
                ));
            }
            ancestor = graph
                .find_thread_spawn_parent(ancestor)
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("controlled ancestry does not reach Main".into())
                })?;
            depth = depth
                .checked_add(1)
                .ok_or_else(|| CodexErr::InvalidRequest("agent depth overflow".into()))?;
        }
        let alias = self.current_agent_alias(thread_id).await?;
        let parent_path = self
            .get_agent_metadata(parent_id)
            .and_then(|metadata| metadata.agent_path);
        let old_path = stored.source.get_agent_path();
        let agent_path = match (parent_path, old_path) {
            (Some(parent), Some(old)) => {
                Some(parent.join(old.name()).map_err(CodexErr::InvalidRequest)?)
            }
            (Some(parent), None) if stored.multi_agent_version == Some(MultiAgentVersion::V2) => {
                Some(
                    parent
                        .join(&format!("agent-{thread_id}"))
                        .map_err(CodexErr::InvalidRequest)?,
                )
            }
            (_, path) => path,
        };
        let agent_role = stored.agent_role.or_else(|| fallback.get_agent_role());
        Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: parent_id,
            depth,
            agent_path,
            agent_nickname: alias
                .and_then(|alias| alias.nickname)
                .or(stored.agent_nickname),
            agent_role,
        }))
    }
}
