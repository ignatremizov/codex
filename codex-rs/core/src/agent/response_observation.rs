use serde::Deserialize;
use serde::Deserializer;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FinalResponseObservation {
    None,
    PresentationOnly,
    #[default]
    Passive,
    Wake,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResponseObservationPolicy {
    commentary: bool,
    final_response: FinalResponseObservation,
}

impl ResponseObservationPolicy {
    pub(crate) fn from_parts(commentary: bool, final_response: FinalResponseObservation) -> Self {
        Self {
            commentary,
            final_response,
        }
    }

    pub(crate) fn commentary(self) -> bool {
        self.commentary
    }

    pub(crate) fn final_response(self) -> FinalResponseObservation {
        self.final_response
    }

    pub(crate) fn wake_on_completion_item_value(self) -> Option<bool> {
        match self.final_response {
            FinalResponseObservation::None | FinalResponseObservation::PresentationOnly => None,
            FinalResponseObservation::Passive => Some(false),
            FinalResponseObservation::Wake => Some(true),
        }
    }

    fn from_wire(value: &str) -> Result<Self, String> {
        match value {
            "c" => Ok(Self {
                commentary: true,
                final_response: FinalResponseObservation::Passive,
            }),
            "f" => Ok(Self {
                commentary: false,
                final_response: FinalResponseObservation::Wake,
            }),
            "cf" => Ok(Self {
                commentary: true,
                final_response: FinalResponseObservation::Wake,
            }),
            "x" => Ok(Self {
                commentary: false,
                final_response: FinalResponseObservation::PresentationOnly,
            }),
            "cx" => Ok(Self {
                commentary: true,
                final_response: FinalResponseObservation::PresentationOnly,
            }),
            "fx" => Ok(Self::default()),
            "cfx" => Ok(Self {
                commentary: true,
                final_response: FinalResponseObservation::Passive,
            }),
            _ => Err(format!(
                "invalid wake/event state `{value}`; expected one of c, f, cf, x, cx, fx, or cfx"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for ResponseObservationPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value).map_err(serde::de::Error::custom)
    }
}

impl From<FinalResponseObservation> for codex_protocol::protocol::AgentResponseFinalDelivery {
    fn from(value: FinalResponseObservation) -> Self {
        match value {
            FinalResponseObservation::None => Self::None,
            FinalResponseObservation::PresentationOnly => Self::PresentationOnly,
            FinalResponseObservation::Passive => Self::Passive,
            FinalResponseObservation::Wake => Self::Wake,
        }
    }
}

impl From<codex_protocol::protocol::AgentResponseFinalDelivery> for FinalResponseObservation {
    fn from(value: codex_protocol::protocol::AgentResponseFinalDelivery) -> Self {
        match value {
            codex_protocol::protocol::AgentResponseFinalDelivery::None => Self::None,
            codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly => {
                Self::PresentationOnly
            }
            codex_protocol::protocol::AgentResponseFinalDelivery::Passive => Self::Passive,
            codex_protocol::protocol::AgentResponseFinalDelivery::Wake => Self::Wake,
        }
    }
}

#[cfg(test)]
#[path = "response_observation_tests.rs"]
mod tests;
