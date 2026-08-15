//! Process-local projection of live agent response observation for the `/agent` pane.

use std::collections::HashMap;

use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::ThreadId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentResponseObservationBinding {
    NextTurn,
    Bound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentFinalResponseDisplay {
    Passive,
    Wake,
    Presentation,
}

impl AgentFinalResponseDisplay {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::Wake => "wake",
            Self::Presentation => "presentation",
        }
    }

    fn strongest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Wake, _) | (_, Self::Wake) => Self::Wake,
            (Self::Passive, _) | (_, Self::Passive) => Self::Passive,
            (Self::Presentation, Self::Presentation) => Self::Presentation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AgentResponseObservationDisplay {
    pub(super) binding: AgentResponseObservationBinding,
    pub(super) commentary: bool,
    pub(super) final_response: AgentFinalResponseDisplay,
}

impl AgentResponseObservationDisplay {
    fn new(
        binding: AgentResponseObservationBinding,
        response_handling: Option<AgentResponseHandling>,
    ) -> Self {
        let (commentary, final_response) = match response_handling {
            None => (false, AgentFinalResponseDisplay::Passive),
            Some(AgentResponseHandling::Commentary) => (true, AgentFinalResponseDisplay::Passive),
            Some(AgentResponseHandling::Wake) => (false, AgentFinalResponseDisplay::Wake),
            Some(AgentResponseHandling::Presentation) => {
                (false, AgentFinalResponseDisplay::Presentation)
            }
            Some(AgentResponseHandling::CommentaryWake) => (true, AgentFinalResponseDisplay::Wake),
            Some(AgentResponseHandling::CommentaryPresentation) => {
                (true, AgentFinalResponseDisplay::Presentation)
            }
        };
        Self {
            binding,
            commentary,
            final_response,
        }
    }

    pub(super) fn compact_label(self) -> String {
        let mut label = if self.commentary {
            format!("commentary · {}", self.final_response.label())
        } else {
            self.final_response.label().to_string()
        };
        if self.binding == AgentResponseObservationBinding::NextTurn {
            label.push_str(" next");
        }
        label
    }

    fn merge(self, other: Self) -> Self {
        Self {
            binding: if matches!(
                (self.binding, other.binding),
                (AgentResponseObservationBinding::Bound, _)
                    | (_, AgentResponseObservationBinding::Bound)
            ) {
                AgentResponseObservationBinding::Bound
            } else {
                AgentResponseObservationBinding::NextTurn
            },
            commentary: self.commentary || other.commentary,
            final_response: self.final_response.strongest(other.final_response),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct AgentResponseObservationState {
    observations: HashMap<(ThreadId, ThreadId), AgentResponseObservationDisplay>,
}

impl AgentResponseObservationState {
    pub(super) fn mark_target_running(&mut self, target_thread_id: ThreadId) {
        for ((_, target), observation) in &mut self.observations {
            if *target == target_thread_id
                && observation.binding == AgentResponseObservationBinding::NextTurn
            {
                observation.binding = AgentResponseObservationBinding::Bound;
            }
        }
    }

    pub(super) fn mark_target_stopped(&mut self, target_thread_id: ThreadId) {
        self.observations.retain(|(_, target), observation| {
            *target != target_thread_id
                || observation.binding == AgentResponseObservationBinding::NextTurn
        });
    }

    pub(super) fn note(
        &mut self,
        observer: ThreadId,
        target: ThreadId,
        binding: AgentResponseObservationBinding,
        response_handling: Option<AgentResponseHandling>,
    ) {
        let observation = AgentResponseObservationDisplay::new(binding, response_handling);
        self.observations
            .entry((observer, target))
            .and_modify(|existing| *existing = existing.merge(observation))
            .or_insert(observation);
    }

    pub(super) fn replace_final_response(
        &mut self,
        observer: ThreadId,
        target: ThreadId,
        binding: AgentResponseObservationBinding,
        final_response: AgentFinalResponseHandling,
    ) {
        let final_response = match final_response {
            AgentFinalResponseHandling::None => {
                self.observations.remove(&(observer, target));
                return;
            }
            AgentFinalResponseHandling::Passive => AgentFinalResponseDisplay::Passive,
            AgentFinalResponseHandling::Wake => AgentFinalResponseDisplay::Wake,
            AgentFinalResponseHandling::Presentation => AgentFinalResponseDisplay::Presentation,
        };
        let commentary = self
            .observations
            .get(&(observer, target))
            .is_some_and(|observation| observation.commentary);
        self.observations.insert(
            (observer, target),
            AgentResponseObservationDisplay {
                binding,
                commentary,
                final_response,
            },
        );
    }

    pub(super) fn get(
        &self,
        observer: ThreadId,
        target: ThreadId,
    ) -> Option<AgentResponseObservationDisplay> {
        self.observations.get(&(observer, target)).copied()
    }

    #[cfg(test)]
    pub(super) fn has_wake(&self, observer: ThreadId, target: ThreadId) -> bool {
        self.observations
            .get(&(observer, target))
            .is_some_and(|observation| {
                observation.final_response == AgentFinalResponseDisplay::Wake
            })
    }

    pub(super) fn remove(&mut self, observer: ThreadId, target: ThreadId) {
        self.observations.remove(&(observer, target));
    }

    pub(super) fn remove_thread(&mut self, thread_id: ThreadId) {
        self.observations
            .retain(|(observer, target), _| *observer != thread_id && *target != thread_id);
    }

    pub(super) fn clear(&mut self) {
        self.observations.clear();
    }
}

#[cfg(test)]
#[path = "agent_observation_display_tests.rs"]
mod tests;
