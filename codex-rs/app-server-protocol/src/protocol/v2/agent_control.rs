use serde::Deserialize;
use serde::Serialize;

use super::UserInput;
use crate::JsonSchema;
use crate::TS;

/// Parameters for a user-authored multi-agent control operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentControlParams {
    /// The thread whose user is authoring the operation and whose model may observe the response.
    pub source_thread_id: String,
    /// Selector text exactly as the user authored it, when the client has that provenance.
    #[ts(optional = nullable)]
    pub authored_selector: Option<String>,
    pub action: AgentControlAction,
}

/// A user-authored multi-agent control operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(tag = "type", rename_all_fields = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentControlAction {
    /// Spawn a default or configured-role child.
    Spawn {
        /// Omitted selects the default child configuration.
        role: Option<String>,
        /// Omitted creates a real idle child without starting its first turn.
        input: Option<Vec<UserInput>>,
        fork_mode: AgentForkMode,
        /// Omitted uses passive final-response delivery.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Send genuine user input to a controlled agent, reopening a known closed descendant.
    Prompt {
        /// Root-scoped ref or nickname, or a canonical thread UUID.
        target: String,
        input: Vec<UserInput>,
        /// Omitted uses passive final-response delivery.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Admit target input using response handling reserved by spawn or resume.
    ReservedPrompt {
        /// Canonical target thread UUID.
        target: String,
        input: Vec<UserInput>,
    },
    /// Queue input as a distinct future target turn, starting it immediately when the target is idle.
    QueuedPrompt {
        /// Root-scoped ref or nickname, or a canonical thread UUID.
        target: String,
        input: Vec<UserInput>,
        /// Omitted uses passive final-response delivery.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Reopen a controlled closed agent or explicitly adopt a stored agent by UUID.
    Resume {
        /// Root-scoped ref or nickname, or a canonical thread UUID for explicit adoption.
        target: String,
        /// Omitted reserves passive delivery for the next admitted target turn.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Interrupt the active target turn and optionally submit a follow-up after cancellation.
    Interrupt {
        target: String,
        input: Option<Vec<UserInput>>,
        /// Valid only when `input` is present.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Close a controlled agent runtime.
    Close {
        target: String,
        /// Omitted replays a completed response passively when it is absent from model context.
        response_handling: Option<AgentResponseHandling>,
    },
    /// Authoritatively replace final-response handling for one target turn.
    Observe {
        target: String,
        response_handling: AgentObservationMode,
    },
}

/// Final-response handling selected by an explicit user observation replacement.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentFinalResponseHandling {
    None,
    Passive,
    Wake,
    Presentation,
}

impl From<codex_protocol::protocol::AgentResponseFinalDelivery> for AgentFinalResponseHandling {
    fn from(value: codex_protocol::protocol::AgentResponseFinalDelivery) -> Self {
        match value {
            codex_protocol::protocol::AgentResponseFinalDelivery::None => Self::None,
            codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly => {
                Self::Presentation
            }
            codex_protocol::protocol::AgentResponseFinalDelivery::Passive => Self::Passive,
            codex_protocol::protocol::AgentResponseFinalDelivery::Wake => Self::Wake,
        }
    }
}

/// Response, reverse-message, and admission handling for one target turn.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentResponseHandling {
    /// Deliver the first complete commentary item from the target turn.
    pub commentary: bool,
    /// Select how the target turn's final response reaches the source.
    pub final_response: AgentFinalResponseHandling,
    /// Let the target turn send attributed input back to the source.
    pub target_messages: bool,
    /// Admit supplied input as a distinct queued target turn and queue its model-visible final
    /// response for the source's next turn.
    pub queue_input: bool,
}

#[allow(non_upper_case_globals)]
impl AgentResponseHandling {
    pub const Commentary: Self = Self::new(
        /*commentary*/ true,
        AgentFinalResponseHandling::Passive,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    pub const Wake: Self = Self::new(
        /*commentary*/ false,
        AgentFinalResponseHandling::Wake,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    pub const Presentation: Self = Self::new(
        /*commentary*/ false,
        AgentFinalResponseHandling::Presentation,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    pub const CommentaryWake: Self = Self::new(
        /*commentary*/ true,
        AgentFinalResponseHandling::Wake,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    pub const CommentaryPresentation: Self = Self::new(
        /*commentary*/ true,
        AgentFinalResponseHandling::Presentation,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );

    /// Builds independent handling for one target turn.
    pub const fn new(
        commentary: bool,
        final_response: AgentFinalResponseHandling,
        target_messages: bool,
        queue_input: bool,
    ) -> Self {
        Self {
            commentary,
            final_response,
            target_messages,
            queue_input,
        }
    }
}

/// Authoritative final-response policy accepted by `agent/control` observe.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentObservationMode {
    Passive,
    Wake,
    Presentation,
}

/// Exact target work whose final-response handling was replaced.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentObservationBinding {
    ActiveTurn,
    NextTurn,
    UndeliveredCompletion,
}

/// Parent conversation history copied into a user-spawned child.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(tag = "type", rename_all_fields = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentForkMode {
    None,
    All,
    LastNTurns { turns: u32 },
}

/// Response to a user-authored multi-agent control operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentControlResponse {
    pub outcome: AgentControlOutcome,
    /// The operation committed, but its source-side audit item could not be persisted.
    pub audit_warning: Option<String>,
}

/// Committed outcome of a user-authored multi-agent control operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(tag = "type", rename_all_fields = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AgentControlOutcome {
    Spawned {
        target_thread_id: String,
        #[serde(rename = "ref")]
        #[ts(rename = "ref")]
        agent_ref: Option<String>,
        nickname: Option<String>,
        /// Non-retryable degradation after child input admission.
        post_admission_warning: Option<String>,
    },
    Prompted {
        target_thread_id: String,
        submission_id: String,
        /// Queue admission was selected; an idle target may already be starting this input.
        queued: bool,
        /// Non-retryable degradation after target input admission.
        post_admission_warning: Option<String>,
    },
    ReservedPrompted {
        target_thread_id: String,
        submission_id: String,
        turn_id: String,
        /// Non-retryable degradation after target input admission.
        post_admission_warning: Option<String>,
    },
    Resumed {
        target_thread_id: String,
        #[serde(rename = "ref")]
        #[ts(rename = "ref")]
        agent_ref: Option<String>,
        nickname: Option<String>,
        observation_binding: Option<AgentObservationBinding>,
        /// Degradation that occurred after an exclusive ownership transfer committed.
        post_commit_warning: Option<String>,
    },
    Interrupted {
        target_thread_id: String,
        submission_id: Option<String>,
        /// Non-retryable degradation after follow-up input admission.
        post_admission_warning: Option<String>,
    },
    Closed {
        target_thread_id: String,
    },
    Observed {
        target_thread_id: String,
        previous_response_handling: AgentFinalResponseHandling,
        response_handling: AgentFinalResponseHandling,
        binding: AgentObservationBinding,
    },
}
