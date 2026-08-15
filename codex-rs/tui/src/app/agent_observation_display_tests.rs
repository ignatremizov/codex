use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::*;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

#[test]
fn dispatch_observation_merges_commentary_and_strongest_final_delivery() {
    let observer = thread_id("00000000-0000-0000-0000-000000000101");
    let target = thread_id("00000000-0000-0000-0000-000000000102");
    let mut state = AgentResponseObservationState::default();

    state.note(
        observer,
        target,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::Wake),
    );
    state.note(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::CommentaryPresentation),
    );

    assert_eq!(
        state.get(observer, target),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: true,
            final_response: AgentFinalResponseDisplay::Wake,
        })
    );
}

#[test]
fn explicit_observation_replacement_can_weaken_final_delivery() {
    let observer = thread_id("00000000-0000-0000-0000-000000000101");
    let target = thread_id("00000000-0000-0000-0000-000000000102");
    let mut state = AgentResponseObservationState::default();
    state.note(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::CommentaryWake),
    );

    state.replace_final_response(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        AgentFinalResponseHandling::Presentation,
    );

    assert_eq!(
        state.get(observer, target),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: true,
            final_response: AgentFinalResponseDisplay::Presentation,
        })
    );
}
