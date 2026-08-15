use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle status attached to a directional thread-spawn edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSpawnEdgeStatus {
    /// The child thread is still live or resumable as an open spawned agent.
    Open,
    /// The child thread has been closed from the parent/child graph's perspective.
    Closed,
}

/// Root-scoped alias state projected from ownership and the canonical spawn edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAliasState {
    /// The target remains part of the current live or resumable root.
    Active,
    /// The target was closed but its aliases remain reserved.
    Closed,
    /// Ownership moved to another root while this historical alias remains reserved.
    Transferred,
}

/// Root-scoped short identities for one canonical thread UUID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAlias {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    pub agent_ref: u64,
    pub nickname: Option<String>,
    pub state: AgentAliasState,
}

/// Inputs for atomically allocating a child alias and its current parent edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocateAgentAliasRequest {
    pub session_id: SessionId,
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub nickname: Option<String>,
}

/// Root namespaces participating in a history-bearing fork reservation import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReserveForkAgentAliasesRequest {
    pub source_session_id: SessionId,
    pub fork_session_id: SessionId,
}

/// Inputs for atomically transferring one thread and its persisted subtree into a new root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferAgentAliasRequest {
    pub expected_previous_session_id: Option<SessionId>,
    /// Descendants reserved by the caller before entering the ownership transaction.
    pub expected_descendant_thread_ids: Vec<ThreadId>,
    pub new_session_id: SessionId,
    pub new_parent_thread_id: ThreadId,
    pub thread_id: ThreadId,
    pub nickname: Option<String>,
    pub authored_selector: String,
}

/// Durable alias and ownership details produced by an exclusive transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAliasTransfer {
    AlreadyOwned {
        alias: AgentAlias,
    },
    Transferred {
        alias: AgentAlias,
        previous_session_id: Option<SessionId>,
        previous_parent_thread_id: Option<ThreadId>,
        transferred_at_ms: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn thread_spawn_edge_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Open)
                .expect("open status should serialize"),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Closed)
                .expect("closed status should serialize"),
            "\"closed\""
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"open\"")
                .expect("open status should deserialize"),
            ThreadSpawnEdgeStatus::Open
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"closed\"")
                .expect("closed status should deserialize"),
            ThreadSpawnEdgeStatus::Closed
        );
    }
}
