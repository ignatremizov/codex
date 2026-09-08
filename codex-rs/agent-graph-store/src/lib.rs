//! Storage-neutral aliases, parent/child topology, and explicit send settings for agents.
//!
//! Send settings are authoritative UUID-keyed records, separate from history and transient
//! observation subscriptions. Callers own permission enforcement and input admission.

mod error;
mod local;
mod local_aliases;
mod store;
mod types;

pub use codex_state::AgentSendMode;
pub use codex_state::AgentSendScope;
pub use codex_state::AgentSendSetting;
pub use error::AgentGraphStoreError;
pub use error::AgentGraphStoreResult;
pub use local::LocalAgentGraphStore;
pub use store::AgentGraphStore;
pub use store::AgentGraphStoreFuture;
pub use types::AgentAlias;
pub use types::AgentAliasState;
pub use types::AgentAliasTransfer;
pub use types::AgentTaskPathMapping;
pub use types::AllocateAgentAliasRequest;
pub use types::ReserveForkAgentAliasesRequest;
pub use types::ThreadSpawnEdgeStatus;
pub use types::TransferAgentAliasRequest;

#[cfg(test)]
#[path = "send_settings_tests.rs"]
mod send_settings_tests;
