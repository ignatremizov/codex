use codex_protocol::SessionId;
use codex_protocol::ThreadId;

use crate::AgentAlias;
use crate::AgentAliasState;
use crate::AgentAliasTransfer;
use crate::AgentGraphStoreError;
use crate::AgentGraphStoreFuture;
use crate::AllocateAgentAliasRequest;
use crate::LocalAgentGraphStore;
use crate::ReserveForkAgentAliasesRequest;
use crate::ThreadSpawnEdgeStatus;
use crate::TransferAgentAliasRequest;
use crate::local::to_state_status;

impl LocalAgentGraphStore {
    pub(super) fn ensure_agent_alias_namespace_impl(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        Box::pin(async move {
            self.state_db
                .ensure_agent_alias_namespace(session_id)
                .await
                .map(AgentAlias::from)
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn reserve_agent_aliases_for_fork_impl(
        &self,
        request: ReserveForkAgentAliasesRequest,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .reserve_agent_aliases_for_fork(codex_state::AgentAliasForkReservation {
                    source_session_id: request.source_session_id,
                    fork_session_id: request.fork_session_id,
                })
                .await
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn discard_fork_agent_alias_reservations_impl(
        &self,
        fork_session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            self.state_db
                .discard_fork_agent_alias_reservations(fork_session_id)
                .await
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn allocate_agent_alias_impl(
        &self,
        request: AllocateAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        Box::pin(async move {
            self.state_db
                .allocate_agent_alias(codex_state::AgentAliasAllocation {
                    session_id: request.session_id,
                    parent_thread_id: request.parent_thread_id,
                    child_thread_id: request.child_thread_id,
                    nickname: request.nickname,
                })
                .await
                .map(AgentAlias::from)
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn activate_agent_alias_impl(
        &self,
        request: AllocateAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        Box::pin(async move {
            self.state_db
                .activate_agent_alias(codex_state::AgentAliasAllocation {
                    session_id: request.session_id,
                    parent_thread_id: request.parent_thread_id,
                    child_thread_id: request.child_thread_id,
                    nickname: request.nickname,
                })
                .await
                .map(AgentAlias::from)
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn transfer_agent_alias_impl(
        &self,
        request: TransferAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAliasTransfer> {
        Box::pin(async move {
            self.state_db
                .transfer_agent_alias(codex_state::AgentAliasTransferRequest {
                    expected_previous_session_id: request.expected_previous_session_id,
                    expected_descendant_thread_ids: request.expected_descendant_thread_ids,
                    new_session_id: request.new_session_id,
                    new_parent_thread_id: request.new_parent_thread_id,
                    thread_id: request.thread_id,
                    nickname: request.nickname,
                    authored_selector: request.authored_selector,
                })
                .await
                .map(AgentAliasTransfer::from)
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn set_agent_lifecycle_state_impl(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            self.state_db
                .set_agent_lifecycle_state(session_id, thread_id, to_state_status(status))
                .await
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn find_agent_alias_by_thread_impl(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        Box::pin(async move {
            self.state_db
                .find_agent_alias_by_thread(session_id, thread_id)
                .await
                .map(|alias| alias.map(AgentAlias::from))
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn find_current_agent_alias_by_thread_impl(
        &self,
        thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        Box::pin(async move {
            self.state_db
                .find_current_agent_alias_by_thread(thread_id)
                .await
                .map(|alias| alias.map(AgentAlias::from))
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn find_agent_alias_by_ref_impl(
        &self,
        session_id: SessionId,
        agent_ref: u64,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        Box::pin(async move {
            self.state_db
                .find_agent_alias_by_ref(session_id, agent_ref)
                .await
                .map(|alias| alias.map(AgentAlias::from))
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn find_agent_alias_by_nickname_impl(
        &self,
        session_id: SessionId,
        nickname: &str,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        let nickname = nickname.to_string();
        Box::pin(async move {
            self.state_db
                .find_agent_alias_by_nickname(session_id, &nickname)
                .await
                .map(|alias| alias.map(AgentAlias::from))
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn list_agent_aliases_impl(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<AgentAlias>> {
        Box::pin(async move {
            self.state_db
                .list_agent_aliases(session_id)
                .await
                .map(|aliases| aliases.into_iter().map(AgentAlias::from).collect())
                .map_err(alias_internal_error)
        })
    }

    pub(super) fn list_agent_nickname_reservations_impl(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<String>> {
        Box::pin(async move {
            self.state_db
                .list_agent_nickname_reservations(session_id)
                .await
                .map_err(alias_internal_error)
        })
    }
}

impl From<codex_state::AgentAliasRecord> for AgentAlias {
    fn from(value: codex_state::AgentAliasRecord) -> Self {
        Self {
            session_id: value.session_id,
            thread_id: value.thread_id,
            agent_ref: value.agent_ref,
            nickname: value.nickname,
            state: match value.state {
                codex_state::AgentAliasState::Active => AgentAliasState::Active,
                codex_state::AgentAliasState::Closed => AgentAliasState::Closed,
                codex_state::AgentAliasState::Transferred => AgentAliasState::Transferred,
            },
        }
    }
}

impl From<codex_state::AgentAliasTransfer> for AgentAliasTransfer {
    fn from(value: codex_state::AgentAliasTransfer) -> Self {
        match value {
            codex_state::AgentAliasTransfer::AlreadyOwned { alias } => Self::AlreadyOwned {
                alias: AgentAlias::from(alias),
            },
            codex_state::AgentAliasTransfer::Transferred {
                alias,
                previous_session_id,
                previous_parent_thread_id,
                transferred_at_ms,
            } => Self::Transferred {
                alias: AgentAlias::from(alias),
                previous_session_id,
                previous_parent_thread_id,
                transferred_at_ms,
            },
        }
    }
}

fn alias_internal_error(err: impl std::fmt::Display) -> AgentGraphStoreError {
    AgentGraphStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[path = "local_aliases_tests.rs"]
mod tests;
