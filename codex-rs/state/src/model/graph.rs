use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use strum::AsRefStr;
use strum::Display;
use strum::EnumString;

/// Status attached to a directional thread-spawn edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DirectionalThreadSpawnEdgeStatus {
    Open,
    Closed,
}

/// Root-scoped alias state projected from ownership and the canonical spawn edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentAliasState {
    Active,
    Closed,
    Transferred,
}

/// Persisted root-scoped alias for one canonical agent thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAliasRecord {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    pub agent_ref: u64,
    pub nickname: Option<String>,
    pub state: AgentAliasState,
}

/// Inputs for atomically allocating a child alias and parent edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAliasAllocation {
    pub session_id: SessionId,
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub nickname: Option<String>,
}

/// Root namespaces participating in a history-bearing fork reservation import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentAliasForkReservation {
    pub source_session_id: SessionId,
    pub fork_session_id: SessionId,
}

/// Inputs for an exclusive subtree ownership transfer into a new root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAliasTransferRequest {
    pub expected_previous_session_id: Option<SessionId>,
    pub expected_descendant_thread_ids: Vec<ThreadId>,
    pub new_session_id: SessionId,
    pub new_parent_thread_id: ThreadId,
    pub thread_id: ThreadId,
    pub nickname: Option<String>,
    pub authored_selector: String,
}

/// Durable audit result for an exclusive alias ownership transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAliasTransfer {
    AlreadyOwned {
        alias: AgentAliasRecord,
    },
    Transferred {
        alias: AgentAliasRecord,
        previous_session_id: Option<SessionId>,
        previous_parent_thread_id: Option<ThreadId>,
        transferred_at_ms: i64,
    },
}
