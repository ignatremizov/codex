//! User-authored control-plane operations for live agents.

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_tools::FunctionCallError;

use super::AgentStatus;
use super::control::ReplacedFinalResponseObservationBinding;
use super::next_thread_spawn_depth;
use super::response_observation::FinalResponseObservation;
use super::response_observation::ResponseObservationPolicy;
use crate::CodexThread;
use crate::tools::handlers::multi_agents_common::thread_spawn_source;

mod audit;
mod lifecycle;
mod prompt;
mod spawn;

/// Response handling requested by a user-authored agent operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserAgentResponseHandling {
    /// Deliver the final response to the source model without waking an idle source.
    #[default]
    Passive,
    /// Also deliver the first complete commentary item.
    Commentary,
    /// Deliver the final response and wake an idle source.
    Wake,
    /// Keep the final response presentation-only.
    Presentation,
    /// Deliver first commentary, then wake for the final response.
    CommentaryWake,
    /// Deliver first commentary while keeping the final response presentation-only.
    CommentaryPresentation,
}

/// Conversation history copied into a user-spawned child.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserAgentForkMode {
    /// Start from instructions and settings only.
    #[default]
    None,
    /// Copy the complete effective parent model context.
    All,
    /// Copy the last positive number of parent turns.
    LastNTurns(usize),
}

/// Final-response handling selected by an explicit user observation replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAgentFinalResponseHandling {
    /// No final response was previously observed.
    None,
    /// Deliver without waking an idle source.
    Passive,
    /// Deliver and wake an idle source.
    Wake,
    /// Keep the response presentation-only.
    Presentation,
}

/// Authoritative final-response policy accepted by an explicit user observation command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAgentObservationMode {
    /// Deliver without waking an idle source.
    Passive,
    /// Deliver and wake an idle source.
    Wake,
    /// Keep the response presentation-only.
    Presentation,
}

/// Exact target work whose final-response handling was replaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAgentObservationBinding {
    /// The target's currently running turn.
    ActiveTurn,
    /// The next turn admitted to an idle target.
    NextTurn,
    /// A completed turn whose final delivery is not committed yet.
    UndeliveredCompletion,
}

impl From<ReplacedFinalResponseObservationBinding> for UserAgentObservationBinding {
    fn from(value: ReplacedFinalResponseObservationBinding) -> Self {
        match value {
            ReplacedFinalResponseObservationBinding::ActiveTurn => Self::ActiveTurn,
            ReplacedFinalResponseObservationBinding::NextTurn => Self::NextTurn,
            ReplacedFinalResponseObservationBinding::UndeliveredCompletion => {
                Self::UndeliveredCompletion
            }
        }
    }
}

impl From<FinalResponseObservation> for UserAgentFinalResponseHandling {
    fn from(value: FinalResponseObservation) -> Self {
        match value {
            FinalResponseObservation::None => Self::None,
            FinalResponseObservation::PresentationOnly => Self::Presentation,
            FinalResponseObservation::Passive => Self::Passive,
            FinalResponseObservation::Wake => Self::Wake,
        }
    }
}

impl From<UserAgentResponseHandling> for ResponseObservationPolicy {
    fn from(value: UserAgentResponseHandling) -> Self {
        match value {
            UserAgentResponseHandling::Passive => Self::default(),
            UserAgentResponseHandling::Commentary => {
                Self::from_parts(/*commentary*/ true, FinalResponseObservation::Passive)
            }
            UserAgentResponseHandling::Wake => {
                Self::from_parts(/*commentary*/ false, FinalResponseObservation::Wake)
            }
            UserAgentResponseHandling::Presentation => Self::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::PresentationOnly,
            ),
            UserAgentResponseHandling::CommentaryWake => {
                Self::from_parts(/*commentary*/ true, FinalResponseObservation::Wake)
            }
            UserAgentResponseHandling::CommentaryPresentation => Self::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::PresentationOnly,
            ),
        }
    }
}

impl TryFrom<ResponseObservationPolicy> for UserAgentResponseHandling {
    type Error = CodexErr;

    fn try_from(value: ResponseObservationPolicy) -> CodexResult<Self> {
        match (value.commentary(), value.final_response()) {
            (false, FinalResponseObservation::Passive) => Ok(Self::Passive),
            (true, FinalResponseObservation::Passive) => Ok(Self::Commentary),
            (false, FinalResponseObservation::Wake) => Ok(Self::Wake),
            (true, FinalResponseObservation::Wake) => Ok(Self::CommentaryWake),
            (false, FinalResponseObservation::PresentationOnly) => Ok(Self::Presentation),
            (true, FinalResponseObservation::PresentationOnly) => Ok(Self::CommentaryPresentation),
            (false | true, FinalResponseObservation::None) => Err(CodexErr::InvalidRequest(
                "reserved response observation has already retired".into(),
            )),
        }
    }
}

impl CodexThread {
    /// Resolve a root-scoped alias or canonical UUID without authorizing a lifecycle mutation.
    pub async fn resolve_user_agent_target(&self, target: &str) -> CodexResult<ThreadId> {
        self.session
            .services
            .agent_control
            .resolve_resumable_agent_target(target)
            .await
    }
}

/// Canonical result of admitting a user-authored prompt to a live agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAgentInputOutcome {
    /// The target proved acceptance, even if observation setup subsequently failed.
    Admitted,
    /// The submission was enqueued but its routing acknowledgment was lost.
    Unknown,
}

/// Result of submitting a user-authored prompt; uncertainty never authorizes automatic retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentPromptResult {
    /// Canonical target thread identity.
    pub target_thread_id: ThreadId,
    /// Submission identity, including when its routing outcome could not be proven.
    pub submission_id: String,
    /// Whether the target proved admission or requires canonical reconciliation.
    pub input_outcome: UserAgentInputOutcome,
    /// Whether prompt admission first reopened a closed controlled target.
    pub resumed_target: bool,
    /// Degradation after enqueue; an unknown outcome requires reconciliation, not automatic retry.
    pub post_admission_warning: Option<String>,
}

/// Canonical result of admitting input under a reserved next-turn response policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentReservedPromptResult {
    /// Canonical target thread identity.
    pub target_thread_id: ThreadId,
    /// Submission identity, including when its routing outcome could not be proven.
    pub submission_id: String,
    /// Whether the target proved admission or requires canonical reconciliation.
    pub input_outcome: UserAgentInputOutcome,
    /// Exact target turn, when the admission receipt proved it. Never synthesized on ambiguity.
    pub target_turn_id: Option<String>,
    /// Effective response policy consumed by the admitted target turn.
    pub response_handling: UserAgentResponseHandling,
    /// Degradation after enqueue; an unknown outcome requires reconciliation, not automatic retry.
    pub post_admission_warning: Option<String>,
}

/// Canonical result of resuming or adopting an agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentResumeResult {
    /// Canonical target thread identity.
    pub target_thread_id: ThreadId,
    /// Stable root-scoped compact ref, when durable aliases are available.
    pub agent_ref: Option<u64>,
    /// Authoritative root-scoped nickname, when one is assigned.
    pub nickname: Option<String>,
    /// Target status after the live control relationship is established.
    pub status: AgentStatus,
    /// Exclusive ownership transition committed by an explicit out-of-root adoption.
    pub ownership_transfer: Option<UserAgentOwnershipTransfer>,
    /// Exact target work covered by the response policy when it remains pending.
    pub observation_binding: Option<UserAgentObservationBinding>,
    /// Degradation that occurred after an exclusive ownership transfer committed.
    pub post_commit_warning: Option<String>,
}

/// Root ownership change committed while adopting a stored agent subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentOwnershipTransfer {
    /// Previous root, or `None` for a standalone rollout with no alias namespace.
    pub previous_session_id: Option<SessionId>,
    /// Root that now exclusively controls the adopted subtree.
    pub new_session_id: SessionId,
}

/// Canonical result of spawning a user-controlled agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentSpawnResult {
    /// Canonical child thread identity.
    pub target_thread_id: ThreadId,
    /// Stable root-scoped compact ref.
    pub agent_ref: Option<u64>,
    /// Generated user-facing nickname.
    pub nickname: Option<String>,
    /// Child status after optional first-turn admission.
    pub status: AgentStatus,
    /// `None` for prompt-less creation.
    pub input_outcome: Option<UserAgentInputOutcome>,
    /// Non-retryable degradation that occurred after child input admission.
    pub post_admission_warning: Option<String>,
}

/// Build provenance for an explicit user-created graph edge.
///
/// `agent_max_depth` bounds autonomous model delegation. User `/agent` spawn and adoption still
/// record their actual depth but are not rejected by that model budget.
fn child_session_source(
    source: &CodexThread,
    turn: &crate::TurnContext,
    role: Option<&str>,
    task_name: Option<String>,
) -> CodexResult<codex_protocol::protocol::SessionSource> {
    control_relationship_source(source, turn, role, task_name)
}

/// Build provenance for an existing-target control relationship without treating it as a spawn.
///
/// The returned depth is meaningful only when a caller later creates a new parent edge, such as
/// explicit out-of-root adoption. Live observation and same-root resume preserve the target's
/// existing graph membership, so the source may already be at the configured spawn-depth limit.
fn control_relationship_source(
    source: &CodexThread,
    turn: &crate::TurnContext,
    role: Option<&str>,
    task_name: Option<String>,
) -> CodexResult<codex_protocol::protocol::SessionSource> {
    let child_depth = next_thread_spawn_depth(&turn.session_source);
    thread_spawn_source(
        source.session.thread_id(),
        &turn.session_source,
        child_depth,
        role,
        task_name,
    )
    .map_err(user_control_tool_error)
}

fn user_control_tool_error(error: FunctionCallError) -> CodexErr {
    match error {
        FunctionCallError::RespondToModel(message) => CodexErr::InvalidRequest(message),
        FunctionCallError::Fatal(message) => CodexErr::Fatal(message),
    }
}
