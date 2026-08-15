//! Durable root-scoped aliases and agent target resolution.

use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentAliasState;
use codex_agent_graph_store::AgentAliasTransfer;
use codex_agent_graph_store::AllocateAgentAliasRequest;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_agent_graph_store::TransferAgentAliasRequest;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use tracing::warn;

use super::AgentControl;
use super::AgentStatus;

pub(crate) fn agent_alias_lifecycle_status(
    state: AgentAliasState,
) -> Option<ThreadSpawnEdgeStatus> {
    match state {
        AgentAliasState::Active => Some(ThreadSpawnEdgeStatus::Open),
        AgentAliasState::Closed => Some(ThreadSpawnEdgeStatus::Closed),
        AgentAliasState::Transferred => None,
    }
}

pub(super) enum ThreadSpawnPersistence {
    New,
    /// Internal descendant restoration after the owning control plane has already been selected.
    Resume,
    /// An existing same-root target; ownership must be revalidated under the target lifecycle
    /// boundary before reopening the runtime.
    ControlledResume,
    Transfer {
        expected_previous_session_id: Option<SessionId>,
        reserved_descendant_thread_ids: Option<Vec<ThreadId>>,
        authored_selector: String,
    },
}

pub(crate) struct AgentResumePlan {
    pub(crate) status: AgentStatus,
    pub(crate) current_alias: Option<AgentAlias>,
    pub(crate) ownership: AgentResumeOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentResumeOwnership {
    CurrentRoot,
    Transfer {
        previous_session_id: Option<SessionId>,
    },
}

impl AgentResumeOwnership {
    pub(crate) fn transfers_ownership(self) -> bool {
        matches!(self, Self::Transfer { .. })
    }
}

#[derive(Clone, Copy)]
enum V1AgentTargetScope {
    ControlledOnly,
    AllowUuidAdoption,
}

enum V1AgentTarget {
    Id(ThreadId),
    Ref(u64),
    Nickname(String),
}

impl AgentControl {
    /// Classify a source-relative resume using durable ownership and process-local liveness.
    ///
    /// The target lifecycle operation revalidates this decision under its lock. This initial plan
    /// lets every transport choose the same controlled-resume versus transfer path and reject a
    /// runtime already loaded under another root before doing expensive resume setup.
    pub(crate) async fn plan_agent_resume(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<AgentResumePlan> {
        let status = self.get_status(thread_id).await;
        let current_alias = self.current_agent_alias(thread_id).await?;
        let current_owner = current_alias.as_ref().map(|alias| alias.session_id);
        let known_to_current_root = self.get_agent_metadata(thread_id).is_some();
        let belongs_to_current_root = self
            .bound_session_id()
            .is_some_and(|session_id| current_owner == Some(session_id))
            || (current_owner.is_none() && known_to_current_root);
        if !matches!(status, AgentStatus::NotFound) && !belongs_to_current_root {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {thread_id} is live under another root; close it before adoption"
            )));
        }
        let ownership = if belongs_to_current_root {
            AgentResumeOwnership::CurrentRoot
        } else {
            AgentResumeOwnership::Transfer {
                previous_session_id: current_owner,
            }
        };
        Ok(AgentResumePlan {
            status,
            current_alias,
            ownership,
        })
    }

    pub(super) async fn sync_durable_agent_nickname_reservations(&self) -> CodexResult<()> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(());
        };
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(());
        }
        let aliases = self.list_session_agent_aliases().await?;
        let inherited_reservations = agent_graph_store
            .list_agent_nickname_reservations(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load inherited agent nickname reservations for {}: {err}",
                    session_id
                ))
            })?;
        let nicknames = aliases
            .into_iter()
            .filter_map(|alias| alias.nickname)
            .chain(inherited_reservations);
        self.state.reserve_durable_agent_nicknames(nicknames);
        Ok(())
    }

    pub(super) async fn list_session_agent_aliases(&self) -> CodexResult<Vec<AgentAlias>> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(Vec::new());
        };
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(Vec::new());
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(Vec::new());
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {}: {err}",
                    session_id
                ))
            })?;
        agent_graph_store
            .list_agent_aliases(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load durable agent aliases for {}: {err}",
                    session_id
                ))
            })
    }

    pub(crate) async fn resolve_controlled_v1_agent_target(
        &self,
        target: &str,
    ) -> CodexResult<ThreadId> {
        self.resolve_controlled_agent_target(target).await
    }

    pub(crate) async fn resolve_controlled_agent_target(
        &self,
        target: &str,
    ) -> CodexResult<ThreadId> {
        self.resolve_v1_agent_target(target, V1AgentTargetScope::ControlledOnly)
            .await
    }

    pub(crate) async fn resolve_resumable_v1_agent_target(
        &self,
        target: &str,
    ) -> CodexResult<ThreadId> {
        self.resolve_resumable_agent_target(target).await
    }

    pub(crate) async fn resolve_resumable_agent_target(
        &self,
        target: &str,
    ) -> CodexResult<ThreadId> {
        self.resolve_v1_agent_target(target, V1AgentTargetScope::AllowUuidAdoption)
            .await
    }

    pub(crate) async fn current_agent_owner_session(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<SessionId>> {
        self.current_agent_alias(thread_id)
            .await
            .map(|alias| alias.map(|alias| alias.session_id))
    }

    pub(crate) async fn current_agent_alias(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<AgentAlias>> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(None);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(None);
        }
        if let Some(session_id) = self.bound_session_id() {
            agent_graph_store
                .ensure_agent_alias_namespace(session_id)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to initialize durable agent aliases for {}: {err}",
                        session_id
                    ))
                })?;
        }
        agent_graph_store
            .find_current_agent_alias_by_thread(thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to resolve current owner for agent {thread_id}: {err}"
                ))
            })
    }

    pub(super) async fn find_session_agent_alias(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<AgentAlias>> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(None);
        };
        let Some(session_id) = self.bound_session_id() else {
            return Ok(None);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(None);
        }
        agent_graph_store
            .find_agent_alias_by_thread(session_id, thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load durable agent identity for {thread_id}: {err}"
                ))
            })
    }

    /// Revalidate durable ownership after taking a target lifecycle boundary.
    ///
    /// Selector resolution happens before the operation can await the target lock. An ownership
    /// transfer may commit during that gap, so mutating paths must not rely on the earlier lookup.
    /// Ephemeral children have no durable alias and remain authorized only while this control's
    /// process-local registry owns their live runtime.
    pub(super) async fn require_current_agent_ownership(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };
        let Some(session_id) = self.bound_session_id() else {
            return Ok(());
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(());
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {}: {err}",
                    session_id
                ))
            })?;
        let current = agent_graph_store
            .find_current_agent_alias_by_thread(thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to revalidate current owner for agent {thread_id}: {err}"
                ))
            })?;
        if current
            .as_ref()
            .is_some_and(|alias| alias.session_id == session_id)
        {
            return Ok(());
        }
        if current.is_none()
            && self.get_agent_metadata(thread_id).is_some()
            && let Ok(thread) = state.get_thread(thread_id).await
            && thread.config_snapshot().await.ephemeral
        {
            return Ok(());
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "agent {thread_id} is no longer controlled by this root"
        )))
    }

    async fn resolve_v1_agent_target(
        &self,
        target: &str,
        scope: V1AgentTargetScope,
    ) -> CodexResult<ThreadId> {
        let parsed = parse_v1_agent_target(target)?;
        if let V1AgentTarget::Id(thread_id) = &parsed
            && matches!(scope, V1AgentTargetScope::AllowUuidAdoption)
        {
            return Ok(*thread_id);
        }

        let state = self.upgrade()?;
        let process_local_controlled = match &parsed {
            V1AgentTarget::Id(thread_id) => self.get_agent_metadata(*thread_id).is_some(),
            V1AgentTarget::Ref(_) | V1AgentTarget::Nickname(_) => false,
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return resolve_without_alias_store(
                parsed,
                scope,
                process_local_controlled,
                self.bound_session_id().map(ThreadId::from),
            );
        };
        let Some(session_id) = self.bound_session_id() else {
            return resolve_without_alias_store(parsed, scope, process_local_controlled, None);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return resolve_without_alias_store(
                parsed,
                scope,
                process_local_controlled,
                Some(ThreadId::from(session_id)),
            );
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {}: {err}",
                    session_id
                ))
            })?;

        let alias = match &parsed {
            V1AgentTarget::Id(thread_id) => {
                agent_graph_store
                    .find_agent_alias_by_thread(session_id, *thread_id)
                    .await
            }
            V1AgentTarget::Ref(agent_ref) => {
                agent_graph_store
                    .find_agent_alias_by_ref(session_id, *agent_ref)
                    .await
            }
            V1AgentTarget::Nickname(nickname) => {
                agent_graph_store
                    .find_agent_alias_by_nickname(session_id, nickname)
                    .await
            }
        }
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to resolve agent target {target:?} in root {}: {err}",
                session_id
            ))
        })?;
        let Some(alias) = alias else {
            if let V1AgentTarget::Id(thread_id) = &parsed
                && process_local_controlled
                && let Ok(thread) = state.get_thread(*thread_id).await
                && thread.config_snapshot().await.ephemeral
            {
                // Ephemeral children deliberately have no durable alias. Their UUID remains a
                // controlled target only while this root-local registry owns the live runtime.
                return Ok(*thread_id);
            }
            return Err(CodexErr::UnsupportedOperation(match parsed {
                V1AgentTarget::Id(thread_id) => {
                    format!(
                        "agent {thread_id} is not controlled by this root; use resume_agent to adopt it"
                    )
                }
                V1AgentTarget::Ref(agent_ref) => {
                    format!("agent ref {agent_ref:?} was not found in this root")
                }
                V1AgentTarget::Nickname(nickname) => {
                    format!("agent target {nickname:?} was not found")
                }
            }));
        };
        match alias.state {
            AgentAliasState::Active | AgentAliasState::Closed => Ok(alias.thread_id),
            AgentAliasState::Transferred => Err(CodexErr::UnsupportedOperation(format!(
                "agent target {target:?} was transferred out of this root; use its canonical UUID to inspect or adopt it"
            ))),
        }
    }

    /// Persist a child alias and edge before publishing its runtime.
    ///
    /// The caller must hold the direct parent's lifecycle guard through this call so a
    /// concurrent subtree close cannot take its final membership snapshot first.
    pub(super) async fn persist_thread_spawn_for_source(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
        persistence: ThreadSpawnPersistence,
    ) -> CodexResult<Option<AgentAlias>> {
        let Some(parent_thread_id) = session_source.and_then(SessionSource::parent_thread_id)
        else {
            return Ok(None);
        };
        if child_thread.config_snapshot().await.ephemeral {
            return Ok(None);
        }
        let Ok(state) = self.upgrade() else {
            return Ok(None);
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(None);
        };
        let session_id = match self.bound_session_id() {
            Some(session_id) if agent_graph_store.supports_agent_aliases() => session_id,
            Some(_) | None => {
                // Unbound controls and topology-only stores can retain the parent edge, but there
                // is no durable root-relative namespace in which an alias is authoritative.
                if let Err(err) = agent_graph_store
                    .upsert_thread_spawn_edge(
                        parent_thread_id,
                        child_thread_id,
                        ThreadSpawnEdgeStatus::Open,
                    )
                    .await
                {
                    warn!("failed to persist thread-spawn edge: {err}");
                }
                return Ok(None);
            }
        };

        let request = AllocateAgentAliasRequest {
            session_id,
            parent_thread_id,
            child_thread_id,
            nickname: session_source.and_then(SessionSource::get_nickname),
        };
        let alias = match persistence {
            ThreadSpawnPersistence::New => agent_graph_store.allocate_agent_alias(request).await,
            ThreadSpawnPersistence::Resume | ThreadSpawnPersistence::ControlledResume => {
                agent_graph_store.activate_agent_alias(request).await
            }
            ThreadSpawnPersistence::Transfer {
                expected_previous_session_id,
                reserved_descendant_thread_ids,
                authored_selector,
            } => match reserved_descendant_thread_ids {
                Some(expected_descendant_thread_ids) => {
                    match agent_graph_store
                        .transfer_agent_alias(TransferAgentAliasRequest {
                            expected_previous_session_id,
                            expected_descendant_thread_ids,
                            new_session_id: session_id,
                            new_parent_thread_id: parent_thread_id,
                            thread_id: child_thread_id,
                            nickname: request.nickname.clone(),
                            authored_selector,
                        })
                        .await
                    {
                        Ok(AgentAliasTransfer::AlreadyOwned { .. }) => {
                            agent_graph_store.activate_agent_alias(request).await
                        }
                        Ok(AgentAliasTransfer::Transferred { alias, .. }) => Ok(alias),
                        Err(err) => Err(err),
                    }
                }
                None => Err(codex_agent_graph_store::AgentGraphStoreError::Internal {
                    message: format!(
                        "ownership transfer for {child_thread_id} reached persistence before its \
                         descendant rollout writers were reserved"
                    ),
                }),
            },
        }
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to persist durable alias for spawned agent {child_thread_id}: {err}"
            ))
        })?;
        Ok(Some(alias))
    }

    pub(super) async fn set_persisted_agent_lifecycle_state(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> CodexResult<bool> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(false);
        };
        let manager = self.upgrade()?;
        let Some(agent_graph_store) = manager.agent_graph_store() else {
            return Ok(false);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(false);
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {}: {err}",
                    session_id
                ))
            })?;
        agent_graph_store
            .set_agent_lifecycle_state(session_id, child_thread_id, status)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to persist agent lifecycle for {child_thread_id}: {err}"
                ))
            })
    }

    pub(super) async fn persist_agent_closed(&self, child_thread_id: ThreadId) -> CodexResult<()> {
        if self
            .set_persisted_agent_lifecycle_state(child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await?
        {
            return Ok(());
        }
        let manager = self.upgrade()?;
        let Some(agent_graph_store) = manager.agent_graph_store() else {
            return Ok(());
        };
        agent_graph_store
            .set_thread_spawn_edge_status(child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to persist closed agent state for {child_thread_id}: {err}"
                ))
            })
    }
}

fn resolve_without_alias_store(
    target: V1AgentTarget,
    scope: V1AgentTargetScope,
    process_local_controlled: bool,
    root_thread_id: Option<ThreadId>,
) -> CodexResult<ThreadId> {
    if let V1AgentTarget::Nickname(nickname) = &target
        && nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME)
        && let Some(root_thread_id) = root_thread_id
    {
        return Ok(root_thread_id);
    }
    match (target, scope) {
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::AllowUuidAdoption) => Ok(thread_id),
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::ControlledOnly)
            if process_local_controlled =>
        {
            Ok(thread_id)
        }
        (V1AgentTarget::Id(thread_id), V1AgentTargetScope::ControlledOnly) => {
            Err(CodexErr::UnsupportedOperation(format!(
                "agent {thread_id} is not controlled by this root; use resume_agent to adopt it"
            )))
        }
        (V1AgentTarget::Ref(_) | V1AgentTarget::Nickname(_), _) => {
            Err(CodexErr::UnsupportedOperation(
                "short agent targets are unavailable; use the full agent UUID".to_string(),
            ))
        }
    }
}

fn parse_v1_agent_target(target: &str) -> CodexResult<V1AgentTarget> {
    if let Some(thread_id) = target.strip_prefix("id:") {
        return ThreadId::from_string(thread_id)
            .map(V1AgentTarget::Id)
            .map_err(|err| {
                CodexErr::UnsupportedOperation(format!("invalid agent UUID {thread_id:?}: {err}"))
            });
    }
    if let Some(agent_ref) = target.strip_prefix("ref:") {
        return parse_v1_agent_ref(agent_ref).map(V1AgentTarget::Ref);
    }
    if let Some(nickname) = target.strip_prefix("nick:")
        && nickname.is_empty()
    {
        return Err(CodexErr::UnsupportedOperation(
            "agent nickname cannot be empty".to_string(),
        ));
    }
    if let Some(nickname) = target.strip_prefix("nick:") {
        return Ok(V1AgentTarget::Nickname(nickname.to_string()));
    }
    if let Ok(thread_id) = ThreadId::from_string(target) {
        return Ok(V1AgentTarget::Id(thread_id));
    }
    if target.is_empty() {
        return Err(CodexErr::UnsupportedOperation(
            "agent target cannot be empty".to_string(),
        ));
    }
    if target.chars().all(|ch| ch.is_ascii_digit()) {
        return parse_v1_agent_ref(target).map(V1AgentTarget::Ref);
    }
    Ok(V1AgentTarget::Nickname(target.to_string()))
}

fn parse_v1_agent_ref(agent_ref: &str) -> CodexResult<u64> {
    let agent_ref = agent_ref.parse::<u64>().map_err(|err| {
        CodexErr::UnsupportedOperation(format!("invalid agent ref {agent_ref:?}: {err}"))
    })?;
    if agent_ref == 0 {
        return Err(CodexErr::UnsupportedOperation(
            "agent refs start at 1".to_string(),
        ));
    }
    Ok(agent_ref)
}
