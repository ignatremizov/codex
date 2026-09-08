//! Shared parsing of compact response-handling flags. Wire order is not significant.

/// The supported alphabet for a response-handling surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeEventSurface {
    Agent,
    /// Payload-bearing agent operations that explicitly support mailbox admission.
    AgentMailbox,
    UserShell,
}

/// Final delivery after cancelling `f` and `x` occurrences pairwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeEventFinalDelivery {
    Passive,
    Wake,
    PresentationOnly,
}

/// Normalized response flags, independent of their input order or repetition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeEventFlags {
    pub commentary: bool,
    pub final_delivery: WakeEventFinalDelivery,
    pub target_messages: bool,
    pub queue_input: bool,
    /// Mailbox admission without automatic response delivery.
    pub mailbox_input: bool,
}

impl WakeEventFlags {
    pub fn parse(value: &str, surface: WakeEventSurface) -> Result<Self, String> {
        let alphabet = match surface {
            WakeEventSurface::Agent => "c, f, m, q, or x",
            WakeEventSurface::AgentMailbox => "c, f, m, q, x, or z",
            WakeEventSurface::UserShell => "f, q, or x",
        };
        if value.is_empty() {
            return Err(format!(
                "empty flags; omit w for default handling or use {alphabet}"
            ));
        }
        let mut commentary = false;
        let mut target_messages = false;
        let mut queue_input = false;
        let mut mailbox_input = false;
        let mut wake_count = 0_usize;
        let mut presentation_count = 0_usize;
        for flag in value.chars() {
            match flag {
                'c' if matches!(
                    surface,
                    WakeEventSurface::Agent | WakeEventSurface::AgentMailbox
                ) =>
                {
                    commentary = true
                }
                'm' if matches!(
                    surface,
                    WakeEventSurface::Agent | WakeEventSurface::AgentMailbox
                ) =>
                {
                    target_messages = true
                }
                'z' if surface == WakeEventSurface::AgentMailbox => mailbox_input = true,
                'q' => queue_input = true,
                'f' => wake_count += 1,
                'x' => presentation_count += 1,
                _ => return Err(format!("unknown flag `{flag}`; use {alphabet}")),
            }
        }
        let mut final_delivery = match wake_count.cmp(&presentation_count) {
            std::cmp::Ordering::Less => WakeEventFinalDelivery::PresentationOnly,
            std::cmp::Ordering::Equal => WakeEventFinalDelivery::Passive,
            std::cmp::Ordering::Greater => WakeEventFinalDelivery::Wake,
        };
        if mailbox_input {
            // Compatibility follows normalized delivery: cancelled wakes are harmless,
            // and excess presentation flags have no meaning for mailbox admission.
            if commentary
                || target_messages
                || queue_input
                || final_delivery == WakeEventFinalDelivery::Wake
            {
                return Err(
                    "mailbox flag `z` cannot be combined with c, m, q, or effective wake delivery"
                        .to_string(),
                );
            }
            final_delivery = WakeEventFinalDelivery::Passive;
        }
        Ok(Self {
            commentary,
            final_delivery,
            target_messages,
            queue_input,
            mailbox_input,
        })
    }
}

#[cfg(test)]
#[path = "wake_event_flags_tests.rs"]
mod tests;
