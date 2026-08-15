//! Process-local projection of live agent response observation for the `/agent` pane.

use std::collections::HashMap;

use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::ThreadId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum AgentResponseObservationBinding {
    NextTurn,
    Bound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentFinalResponseDisplay {
    None,
    Passive,
    Wake,
    Presentation,
}

impl AgentFinalResponseDisplay {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::None => "no final",
            Self::Passive => "passive",
            Self::Wake => "wake",
            Self::Presentation => "presentation",
        }
    }

    fn strongest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Wake, _) | (_, Self::Wake) => Self::Wake,
            (Self::Passive, _) | (_, Self::Passive) => Self::Passive,
            (Self::Presentation, _) | (_, Self::Presentation) => Self::Presentation,
            (Self::None, Self::None) => Self::None,
        }
    }
}

impl From<AgentFinalResponseHandling> for AgentFinalResponseDisplay {
    fn from(value: AgentFinalResponseHandling) -> Self {
        match value {
            AgentFinalResponseHandling::None => Self::None,
            AgentFinalResponseHandling::Passive => Self::Passive,
            AgentFinalResponseHandling::Wake => Self::Wake,
            AgentFinalResponseHandling::Presentation => Self::Presentation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AgentResponseObservationDisplay {
    pub(super) binding: AgentResponseObservationBinding,
    pub(super) commentary: bool,
    pub(super) target_messages: bool,
    pub(super) queue_delivery: bool,
    pub(super) final_response: AgentFinalResponseDisplay,
}

impl AgentResponseObservationDisplay {
    fn new(
        binding: AgentResponseObservationBinding,
        response_handling: Option<AgentResponseHandling>,
    ) -> Self {
        let response_handling = response_handling.unwrap_or(AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::Passive,
            /*target_messages*/ false,
            /*queue_input*/ false,
        ));
        Self {
            binding,
            commentary: response_handling.commentary,
            target_messages: response_handling.target_messages,
            queue_delivery: response_handling.queue_input,
            final_response: response_handling.final_response.into(),
        }
    }

    pub(super) fn compact_label(self) -> String {
        let mut labels = Vec::new();
        if self.commentary {
            labels.push("commentary");
        }
        if self.final_response != AgentFinalResponseDisplay::None {
            labels.push(self.final_response.label());
        }
        if self.target_messages {
            labels.push("replies");
        }
        if self.queue_delivery {
            labels.push("queued");
        }
        if labels.is_empty() {
            labels.push(self.final_response.label());
        }
        let mut label = labels.join(" · ");
        if self.binding == AgentResponseObservationBinding::NextTurn {
            label.push_str(" next");
        }
        label
    }

    fn merge(self, other: Self) -> Self {
        debug_assert_eq!(self.binding, other.binding);
        Self {
            binding: self.binding,
            commentary: self.commentary || other.commentary,
            target_messages: self.target_messages || other.target_messages,
            queue_delivery: self.queue_delivery || other.queue_delivery,
            final_response: self.final_response.strongest(other.final_response),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct AgentResponseObservationState {
    observations: HashMap<
        (ThreadId, ThreadId, AgentResponseObservationBinding),
        AgentResponseObservationDisplay,
    >,
}

impl AgentResponseObservationState {
    pub(super) fn mark_target_running(&mut self, target_thread_id: ThreadId) {
        let pending = self
            .observations
            .iter()
            .filter_map(|((observer, target, binding), observation)| {
                (*target == target_thread_id
                    && *binding == AgentResponseObservationBinding::NextTurn)
                    .then_some((*observer, *observation))
            })
            .collect::<Vec<_>>();
        for (observer, mut observation) in pending {
            self.observations.remove(&(
                observer,
                target_thread_id,
                AgentResponseObservationBinding::NextTurn,
            ));
            observation.binding = AgentResponseObservationBinding::Bound;
            self.observations
                .entry((
                    observer,
                    target_thread_id,
                    AgentResponseObservationBinding::Bound,
                ))
                .and_modify(|existing| *existing = existing.merge(observation))
                .or_insert(observation);
        }
    }

    pub(super) fn mark_target_stopped(&mut self, target_thread_id: ThreadId) {
        self.observations.retain(|(_, target, binding), _| {
            *target != target_thread_id || *binding == AgentResponseObservationBinding::NextTurn
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
            .entry((observer, target, binding))
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
        let final_response = final_response.into();
        let key = (observer, target, binding);
        let commentary = self
            .observations
            .get(&key)
            .is_some_and(|observation| observation.commentary);
        let target_messages = self
            .observations
            .get(&key)
            .is_some_and(|observation| observation.target_messages);
        let queue_delivery = self
            .observations
            .get(&key)
            .is_some_and(|observation| observation.queue_delivery);
        if final_response == AgentFinalResponseDisplay::None
            && !commentary
            && !target_messages
            && !queue_delivery
        {
            self.observations.remove(&key);
            return;
        }
        self.observations.insert(
            key,
            AgentResponseObservationDisplay {
                binding,
                commentary,
                target_messages,
                queue_delivery,
                final_response,
            },
        );
    }

    pub(super) fn get(
        &self,
        observer: ThreadId,
        target: ThreadId,
    ) -> Option<AgentResponseObservationDisplay> {
        self.observations
            .get(&(observer, target, AgentResponseObservationBinding::Bound))
            .or_else(|| {
                self.observations.get(&(
                    observer,
                    target,
                    AgentResponseObservationBinding::NextTurn,
                ))
            })
            .copied()
    }

    #[cfg(test)]
    pub(super) fn has_wake(&self, observer: ThreadId, target: ThreadId) -> bool {
        self.get(observer, target).is_some_and(|observation| {
            observation.final_response == AgentFinalResponseDisplay::Wake
        })
    }

    pub(super) fn remove(&mut self, observer: ThreadId, target: ThreadId) {
        self.observations
            .retain(|(candidate_observer, candidate_target, _), _| {
                *candidate_observer != observer || *candidate_target != target
            });
    }

    pub(super) fn remove_binding(
        &mut self,
        observer: ThreadId,
        target: ThreadId,
        binding: AgentResponseObservationBinding,
    ) {
        self.observations.remove(&(observer, target, binding));
    }

    pub(super) fn remove_thread(&mut self, thread_id: ThreadId) {
        self.observations
            .retain(|(observer, target, _), _| *observer != thread_id && *target != thread_id);
    }

    pub(super) fn clear(&mut self) {
        self.observations.clear();
    }
}

#[cfg(test)]
#[path = "agent_observation_display_tests.rs"]
mod tests;
