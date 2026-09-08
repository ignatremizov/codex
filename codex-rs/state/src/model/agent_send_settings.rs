use codex_protocol::ThreadId;

/// Canonical scope of an explicit messaging setting, independent of runtime presentations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentSendScope {
    Directed {
        sender_thread_id: ThreadId,
        receiver_thread_id: ThreadId,
    },
    Subtree {
        supervisor_thread_id: ThreadId,
    },
}

/// Explicit policy; an absent setting is distinct from a disabled setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSendMode {
    Enabled,
    Disabled,
}

/// Authoritative stored setting. Revision is per scope and increases on every write.
/// It is refresh metadata, not a global snapshot version or a caller precondition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSendSetting {
    pub scope: AgentSendScope,
    pub mode: AgentSendMode,
    pub revision: i64,
}
