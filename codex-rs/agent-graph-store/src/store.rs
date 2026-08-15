use std::future::Future;
use std::pin::Pin;

use codex_protocol::SessionId;
use codex_protocol::ThreadId;

use crate::AgentAlias;
use crate::AgentAliasTransfer;
use crate::AgentGraphStoreError;
use crate::AgentGraphStoreResult;
use crate::AllocateAgentAliasRequest;
use crate::ReserveForkAgentAliasesRequest;
use crate::ThreadSpawnEdgeStatus;
use crate::TransferAgentAliasRequest;

/// Future returned by [`AgentGraphStore`] operations.
pub type AgentGraphStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = AgentGraphStoreResult<T>> + Send + 'a>>;

/// Storage-neutral boundary for persisted agent aliases and parent/child topology.
///
/// Implementations that support aliases must allocate refs monotonically and retain closed or
/// transferred aliases so historical short targets can never be rebound to a different thread.
/// Alias allocation and the corresponding parent edge must commit atomically.
///
/// Implementations that only provide graph traversal may retain the default unsupported alias
/// methods.
pub trait AgentGraphStore: Send + Sync {
    /// Whether this store implements the durable alias operations below.
    fn supports_agent_aliases(&self) -> bool {
        false
    }

    /// Ensure that the root namespace exists and return its ref-1 Main alias.
    fn ensure_agent_alias_namespace(
        &self,
        _session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        unsupported_alias_store()
    }

    /// Reserve inherited refs and nicknames without copying live target mappings.
    fn reserve_agent_aliases_for_fork(
        &self,
        _request: ReserveForkAgentAliasesRequest,
    ) -> AgentGraphStoreFuture<'_, ()> {
        unsupported_alias_store()
    }

    /// Discard an unpublished fork's isolated alias namespace and inherited reservations.
    ///
    /// Implementations must refuse removal after the namespace gains child aliases or transfer
    /// history, so stale cleanup cannot erase a published fork's durable identity.
    fn discard_fork_agent_alias_reservations(
        &self,
        _fork_session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, bool> {
        unsupported_alias_store()
    }

    /// Atomically allocate a child alias and persist its current parent edge.
    ///
    /// Repeating the same `(session_id, child_thread_id)` request is idempotent and returns the
    /// existing alias.
    fn allocate_agent_alias(
        &self,
        _request: AllocateAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        unsupported_alias_store()
    }

    /// Reopen a closed alias and its existing parent edge, or allocate it when absent.
    ///
    /// A supplied nickname may fill an alias whose historical metadata had none, but never
    /// replaces an already persisted nickname.
    fn activate_agent_alias(
        &self,
        _request: AllocateAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        unsupported_alias_store()
    }

    /// Exclusively transfer one target and its persisted subtree into a new root.
    ///
    /// The expected previous owner protects concurrent adoption attempts from silently stealing
    /// ownership from the first committed winner. The required descendant snapshot verifies that
    /// the subtree did not change after the caller reserved its rollout writers. Descendant aliases
    /// move in the same transaction so the replaced parent edge cannot expose a subtree still owned
    /// by the former root.
    fn transfer_agent_alias(
        &self,
        _request: TransferAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAliasTransfer> {
        unsupported_alias_store()
    }

    /// Change ordinary active/closed lifecycle state without releasing either reservation.
    ///
    /// Ownership transfer is intentionally rejected here and belongs to a separate atomic
    /// transfer operation.
    fn set_agent_lifecycle_state(
        &self,
        _session_id: SessionId,
        _thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, bool> {
        unsupported_alias_store()
    }

    /// Find an alias by canonical thread UUID.
    fn find_agent_alias_by_thread(
        &self,
        _session_id: SessionId,
        _thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        unsupported_alias_store()
    }

    /// Find the active or closed alias that currently owns a canonical thread UUID.
    fn find_current_agent_alias_by_thread(
        &self,
        _thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        unsupported_alias_store()
    }

    /// Find an alias by its exact root-scoped numeric ref.
    fn find_agent_alias_by_ref(
        &self,
        _session_id: SessionId,
        _agent_ref: u64,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        unsupported_alias_store()
    }

    /// Find an alias by root-scoped nickname.
    ///
    /// Implementations match ordinary nicknames exactly and the reserved Main nickname
    /// case-insensitively.
    fn find_agent_alias_by_nickname(
        &self,
        _session_id: SessionId,
        _nickname: &str,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        unsupported_alias_store()
    }

    /// List aliases in stable numeric-ref order, including retained reservations.
    fn list_agent_aliases(
        &self,
        _session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<AgentAlias>> {
        unsupported_alias_store()
    }

    /// List inherited nickname reservations that do not resolve to live targets.
    fn list_agent_nickname_reservations(
        &self,
        _session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<String>> {
        unsupported_alias_store()
    }

    /// Insert or replace the directional parent/child edge for a spawned thread.
    ///
    /// `child_thread_id` has at most one persisted parent. Re-inserting the same child should
    /// update both the parent and status to match the supplied values.
    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()>;

    /// Update the persisted lifecycle status of a spawned thread's incoming edge.
    ///
    /// Implementations should treat missing children as a successful no-op.
    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()>;

    /// Find the direct persisted parent of a spawned thread.
    fn find_thread_spawn_parent(
        &self,
        child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadId>>;

    /// List direct spawned children of a parent thread.
    ///
    /// When `status_filter` is `Some`, only child edges with that exact status are returned. When
    /// it is `None`, all direct child edges are returned regardless of status, including statuses
    /// that may be added by a future store implementation.
    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>>;

    /// List spawned descendants breadth-first by depth, then by thread id.
    ///
    /// `status_filter` is applied to every traversed edge, not just to the returned descendants.
    /// For example, `Some(Open)` walks only open edges, so descendants under a closed edge are not
    /// included even if their own incoming edge is open. `None` walks and returns every persisted
    /// edge regardless of status.
    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>>;
}

fn unsupported_alias_store<T>() -> AgentGraphStoreFuture<'static, T> {
    Box::pin(async {
        Err(AgentGraphStoreError::InvalidRequest {
            message: "durable agent aliases are unavailable for this graph store".to_string(),
        })
    })
}
