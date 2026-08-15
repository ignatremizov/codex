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
use codex_thread_store::ReadThreadParams;
use tracing::warn;

use super::AgentStatus;
use super::LocalAgentControl;
mod persistence;
mod target;
use target::parse_v1_agent_target;
use target::resolve_without_alias_store;

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

/// Durable publication result; callers must not undo a committed ownership transfer.
#[derive(Default)]
pub(super) struct PersistedAgentSpawn {
    pub(super) alias: Option<AgentAlias>,
    pub(super) transfer: Option<AgentAliasTransfer>,
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

impl LocalAgentControl {
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
            || (current_owner.is_none()
                && (known_to_current_root || self.bound_session_id().is_none()));
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
                    "failed to load inherited agent nickname reservations for {session_id}: {err}"
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
                    "failed to initialize durable agent aliases for {session_id}: {err}"
                ))
            })?;
        agent_graph_store
            .list_agent_aliases(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load durable agent aliases for {session_id}: {err}"
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
                        "failed to initialize durable agent aliases for {session_id}: {err}"
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
                    "failed to initialize durable agent aliases for {session_id}: {err}"
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

        let state = match self.upgrade() {
            Ok(state) => state,
            Err(_) => {
                return resolve_without_alias_store(
                    parsed,
                    scope,
                    /*process_local_controlled*/ false,
                    /*thread_exists*/ false,
                    self.bound_session_id().map(ThreadId::from),
                );
            }
        };
        let (process_local_controlled, thread_exists) = match &parsed {
            V1AgentTarget::Id(thread_id) => {
                let has_metadata = self.get_agent_metadata(*thread_id).is_some();
                let is_unbound_local =
                    self.bound_session_id().is_none() && state.get_thread(*thread_id).await.is_ok();
                let exists = has_metadata
                    || is_unbound_local
                    || state.get_thread(*thread_id).await.is_ok()
                    || state
                        .read_stored_thread(ReadThreadParams {
                            thread_id: *thread_id,
                            include_archived: true,
                            include_history: false,
                        })
                        .await
                        .is_ok();
                (has_metadata || is_unbound_local, exists)
            }
            V1AgentTarget::Ref(_) | V1AgentTarget::Nickname(_) => (false, false),
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return resolve_without_alias_store(
                parsed,
                scope,
                process_local_controlled,
                thread_exists,
                self.bound_session_id().map(ThreadId::from),
            );
        };
        let Some(session_id) = self.bound_session_id() else {
            return resolve_without_alias_store(
                parsed,
                scope,
                process_local_controlled,
                thread_exists,
                /*root_thread_id*/ None,
            );
        };
        if !agent_graph_store.supports_agent_aliases() {
            return resolve_without_alias_store(
                parsed,
                scope,
                process_local_controlled,
                thread_exists,
                Some(ThreadId::from(session_id)),
            );
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {session_id}: {err}"
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
                "failed to resolve agent target {target:?} in root {session_id}: {err}"
            ))
        })?;
        let Some(alias) = alias else {
            if let V1AgentTarget::Id(thread_id) = &parsed
                && process_local_controlled
                && let Ok(thread) = state.get_thread(*thread_id).await
                && thread.config_snapshot().await.ephemeral
            {
                return Ok(*thread_id);
            }
            return Err(match parsed {
                V1AgentTarget::Id(thread_id) => {
                    if agent_graph_store
                        .find_current_agent_alias_by_thread(thread_id)
                        .await
                        .map_err(|error| {
                            CodexErr::Fatal(format!("failed to resolve agent owner: {error}"))
                        })?
                        .is_some()
                        || thread_exists
                    {
                        CodexErr::UnsupportedOperation(format!(
                            "agent {thread_id} is not controlled by this root; use resume_agent to adopt it"
                        ))
                    } else {
                        CodexErr::ThreadNotFound(thread_id)
                    }
                }
                V1AgentTarget::Ref(agent_ref) => CodexErr::UnsupportedOperation(format!(
                    "agent ref {agent_ref:?} was not found in this root"
                )),
                V1AgentTarget::Nickname(nickname) => CodexErr::UnsupportedOperation(format!(
                    "agent target {nickname:?} was not found"
                )),
            });
        };
        match alias.state {
            AgentAliasState::Active | AgentAliasState::Closed => {
                let current = agent_graph_store
                    .find_current_agent_alias_by_thread(alias.thread_id)
                    .await
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?;
                if current.as_ref().map(|current| current.session_id) != Some(session_id) {
                    return Err(CodexErr::UnsupportedOperation(format!(
                        "agent target {target:?} is deleted or no longer controlled by this root"
                    )));
                }
                Ok(alias.thread_id)
            }
            AgentAliasState::Transferred => Err(CodexErr::UnsupportedOperation(format!(
                "agent target {target:?} was transferred out of this root; use its canonical UUID to inspect or adopt it"
            ))),
        }
    }
}
