//! Storage-neutral aliases and parent/child topology for thread-spawned agents.

mod error;
mod local;
mod local_aliases;
mod store;
mod types;

pub use error::AgentGraphStoreError;
pub use error::AgentGraphStoreResult;
pub use local::LocalAgentGraphStore;
pub use store::AgentGraphStore;
pub use store::AgentGraphStoreFuture;
pub use types::AgentAlias;
pub use types::AgentAliasState;
pub use types::AgentAliasTransfer;
pub use types::AllocateAgentAliasRequest;
pub use types::ReserveForkAgentAliasesRequest;
pub use types::ThreadSpawnEdgeStatus;
pub use types::TransferAgentAliasRequest;
