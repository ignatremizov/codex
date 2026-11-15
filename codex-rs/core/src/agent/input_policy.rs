//! Shared agent-input policy, independent of tool names and runtime implementation.
//!
//! Omission stays distinct from explicit passive delivery: the selected controller resolves
//! directional defaults for each receiver. This parser grants neither runtime nor mailbox authority.

use super::response_observation::FinalResponseObservation;
use super::response_observation::ResponseObservationPolicy;
use codex_protocol::WakeEventFinalDelivery;
use codex_protocol::WakeEventFlags;
use codex_protocol::WakeEventMailboxSubscription;
use codex_protocol::WakeEventSurface;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SendInputMode {
    /// Resolve omitted response handling against each recipient's relationship to the sender.
    #[default]
    Default,
    Response(ResponseObservationPolicy),
    Mailbox(WakeEventMailboxSubscription),
}

impl SendInputMode {
    pub(crate) fn response_observation(self) -> ResponseObservationPolicy {
        match self {
            Self::Default => ResponseObservationPolicy::default(),
            Self::Response(policy) => policy,
            Self::Mailbox(_) => ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
            ),
        }
    }

    pub(crate) fn normalized_flags(self) -> String {
        match self {
            Self::Default => String::new(),
            Self::Mailbox(WakeEventMailboxSubscription::None) => "z".to_string(),
            Self::Mailbox(WakeEventMailboxSubscription::Wake) => "zf".to_string(),
            Self::Response(policy) => {
                let mut flags = String::new();
                if policy.commentary() {
                    flags.push('c');
                }
                match policy.final_response() {
                    FinalResponseObservation::None | FinalResponseObservation::Passive => {}
                    FinalResponseObservation::Wake => flags.push('f'),
                    FinalResponseObservation::PresentationOnly => flags.push('x'),
                }
                if policy.queue_input() {
                    flags.push('q');
                }
                flags
            }
        }
    }
}

impl<'de> Deserialize<'de> for SendInputMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let flags =
            WakeEventFlags::parse(&value, WakeEventSurface::AgentMailbox).map_err(|error| {
                serde::de::Error::custom(format!("invalid wake/event state `{value}`; {error}"))
            })?;
        if flags.mailbox_input {
            return Ok(Self::Mailbox(flags.mailbox_subscription));
        }
        let final_response = match flags.final_delivery {
            WakeEventFinalDelivery::Passive => FinalResponseObservation::Passive,
            WakeEventFinalDelivery::Wake => FinalResponseObservation::Wake,
            WakeEventFinalDelivery::PresentationOnly => FinalResponseObservation::PresentationOnly,
        };
        Ok(Self::Response(ResponseObservationPolicy::from_turn_parts(
            flags.commentary,
            final_response,
            flags.queue_input,
        )))
    }
}

#[cfg(test)]
#[path = "input_policy_tests.rs"]
mod tests;
