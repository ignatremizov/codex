//! A user answer keeps the server and agent that requested it when TUI focus changes.

use crate::backend_id::BackendId;
use codex_protocol::ThreadId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentQuestionBinding {
    pub(crate) backend_id: BackendId,
    pub(crate) source_thread_id: ThreadId,
    pub(crate) requester_thread_id: ThreadId,
    pub(crate) requester_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentQuestionDelivery {
    Direct,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentQuestionAnswer {
    pub(crate) binding: AgentQuestionBinding,
    pub(crate) question_id: String,
    pub(crate) text: String,
    pub(crate) delivery: AgentQuestionDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentQuestionAnswerOutcome {
    Accepted,
    Rejected,
    Uncertain,
}
