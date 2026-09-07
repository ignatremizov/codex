//! Shared parsing of compact response-handling flags. Wire order is not significant.

/// The supported alphabet for a response-handling surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeEventSurface {
    Agent,
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
}

impl WakeEventFlags {
    pub fn parse(value: &str, surface: WakeEventSurface) -> Result<Self, String> {
        let alphabet = match surface {
            WakeEventSurface::Agent => "c, f, m, q, or x",
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
        let mut wake_count = 0_usize;
        let mut presentation_count = 0_usize;
        for flag in value.chars() {
            match flag {
                'c' if surface == WakeEventSurface::Agent => commentary = true,
                'm' if surface == WakeEventSurface::Agent => target_messages = true,
                'q' => queue_input = true,
                'f' => wake_count += 1,
                'x' => presentation_count += 1,
                _ => return Err(format!("unknown flag `{flag}`; use {alphabet}")),
            }
        }
        let final_delivery = match wake_count.cmp(&presentation_count) {
            std::cmp::Ordering::Less => WakeEventFinalDelivery::PresentationOnly,
            std::cmp::Ordering::Equal => WakeEventFinalDelivery::Passive,
            std::cmp::Ordering::Greater => WakeEventFinalDelivery::Wake,
        };
        Ok(Self {
            commentary,
            final_delivery,
            target_messages,
            queue_input,
        })
    }
}

#[cfg(test)]
#[path = "wake_event_flags_tests.rs"]
mod tests;
