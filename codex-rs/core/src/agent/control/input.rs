//! Trusted presentation provenance is carried separately from model-visible input.

use super::*;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::turn_input::TurnInput;

#[derive(Clone)]
pub(crate) enum AgentControlInput {
    User(Vec<UserInput>),
    AttributedAgent {
        content: Vec<UserInput>,
        transcript: String,
        presentation: Vec<UserInput>,
    },
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;

impl AgentControlInput {
    pub(crate) fn presentation(&self) -> &[UserInput] {
        match self {
            Self::User(input) => input,
            Self::AttributedAgent { presentation, .. } => presentation,
        }
    }

    pub(super) fn into_request(self) -> TurnInputRequest {
        match self {
            Self::User(content) => TurnInputRequest::user_input(content),
            Self::AttributedAgent {
                content,
                transcript,
                ..
            } => TurnInputRequest::new(TurnInput::AgentInput {
                content,
                presentation: AgentInputPresentation::Attributed(transcript),
            }),
        }
    }
}
