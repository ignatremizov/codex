//! Trusted presentation provenance is carried separately from model-visible input.

use super::*;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::turn_input::TurnInput;

#[derive(Clone)]
pub(crate) enum AgentControlInput {
    User(Vec<UserInput>),
    PreparedAgentInput(Box<codex_protocol::PreparedAgentInput>),
    AttributedAgentInput {
        content: Vec<UserInput>,
        attribution: Box<codex_protocol::AgentInputAttribution>,
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
            Self::AttributedAgentInput { presentation, .. } => presentation,
            // Opaque prepared input is not a vector of human or structured user items.
            Self::PreparedAgentInput(_) => &[],
        }
    }

    pub(crate) fn preview(&self) -> String {
        match self {
            Self::PreparedAgentInput(input) => {
                input.readable_content().unwrap_or_default().to_owned()
            }
            Self::User(_) | Self::AttributedAgentInput { .. } => {
                render_input_preview(self.presentation())
            }
        }
    }

    pub(super) fn into_request(self) -> TurnInputRequest {
        match self {
            Self::User(content) => TurnInputRequest::user_input(content),
            Self::PreparedAgentInput(input) => {
                TurnInputRequest::new(TurnInput::PreparedAgentInput(input))
            }
            Self::AttributedAgentInput {
                content,
                attribution,
                presentation,
            } => TurnInputRequest::new(TurnInput::AgentInput {
                content,
                presentation: AgentInputPresentation::AttributedInput {
                    attribution,
                    input: presentation,
                },
                context: Vec::new(),
            }),
        }
    }
}
