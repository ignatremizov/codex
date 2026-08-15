use codex_protocol::ThreadId;
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
    target_messages: bool,
    queue_input: bool,
}

impl ResponseObservationPolicy {
    pub(crate) fn from_parts(commentary: bool, final_response: FinalResponseObservation) -> Self {
        Self::from_turn_parts(
            commentary,
            final_response,
            /*target_messages*/ false,
            /*queue_input*/ false,
        )
    }

    pub(crate) fn from_turn_parts(
        commentary: bool,
        final_response: FinalResponseObservation,
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

    pub(crate) fn commentary(self) -> bool {
        self.commentary
    }

    pub(crate) fn final_response(self) -> FinalResponseObservation {
        self.final_response
    }

    pub(crate) fn target_messages(self) -> bool {
        self.target_messages
    }

    /// Whether input and final delivery use their respective next-turn queues.
    pub(crate) fn queue_input(self) -> bool {
        self.queue_input
    }

    pub(crate) fn admitted_queue_turn_metadata(
        self,
        queue_id: String,
        source_thread_id: ThreadId,
    ) -> codex_protocol::protocol::AgentQueueTurnMetadata {
        codex_protocol::protocol::AgentQueueTurnMetadata {
            queue_id,
            source_thread_id,
            response_handling: Some(codex_protocol::protocol::AgentQueueResponseHandling {
                commentary: self.commentary,
                final_delivery: self.final_response.into(),
                target_messages: self.target_messages,
            }),
        }
    }

    pub(crate) fn exposes_source_model_context(self) -> bool {
        self.target_messages
            || self.commentary
            || matches!(
                self.final_response,
                FinalResponseObservation::Passive | FinalResponseObservation::Wake
            )
    }

    pub(crate) fn wake_on_completion_item_value(self) -> Option<bool> {
        match self.final_response {
            FinalResponseObservation::None | FinalResponseObservation::PresentationOnly => None,
            FinalResponseObservation::Passive => Some(false),
            FinalResponseObservation::Wake => Some(true),
        }
    }

    fn from_wire(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("invalid wake/event state ``; omit w for default handling".to_string());
        }

        let mut commentary = false;
        let mut final_wake = false;
        let mut target_messages = false;
        let mut queue_input = false;
        let mut presentation_only = false;
        let mut previous_position = None;
        for flag in value.chars() {
            let position = match flag {
                'c' => {
                    if commentary {
                        return Err(invalid_policy(value));
                    }
                    commentary = true;
                    0
                }
                'f' => {
                    if final_wake {
                        return Err(invalid_policy(value));
                    }
                    final_wake = true;
                    1
                }
                'm' => {
                    if target_messages {
                        return Err(invalid_policy(value));
                    }
                    target_messages = true;
                    2
                }
                'q' => {
                    if queue_input {
                        return Err(invalid_policy(value));
                    }
                    queue_input = true;
                    3
                }
                'x' => {
                    if presentation_only {
                        return Err(invalid_policy(value));
                    }
                    presentation_only = true;
                    4
                }
                _ => return Err(invalid_policy(value)),
            };
            if previous_position.is_some_and(|previous| position <= previous) {
                return Err(invalid_policy(value));
            }
            previous_position = Some(position);
        }

        let final_response = match (final_wake, presentation_only) {
            (true, false) => FinalResponseObservation::Wake,
            (false, true) => FinalResponseObservation::PresentationOnly,
            (false, false) | (true, true) => FinalResponseObservation::Passive,
        };
        Ok(Self {
            commentary,
            final_response,
            target_messages,
            queue_input,
        })
    }
}

fn invalid_policy(value: &str) -> String {
    format!("invalid wake/event state `{value}`; use unique c, f, m, q, or x flags in cfmqx order")
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
