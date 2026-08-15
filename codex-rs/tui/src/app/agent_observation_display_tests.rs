use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::*;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

#[test]
fn bound_observation_precedes_and_preserves_next_turn_policy() {
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
            target_messages: false,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Presentation,
        })
    );

    state.mark_target_stopped(target);

    assert_eq!(
        state.get(observer, target),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::NextTurn,
            commentary: false,
            target_messages: false,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Wake,
        })
    );
}

#[test]
fn removing_completed_binding_preserves_next_turn_policy() {
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
        Some(AgentResponseHandling::Presentation),
    );

    state.remove_binding(observer, target, AgentResponseObservationBinding::Bound);

    assert_eq!(
        state.get(observer, target),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::NextTurn,
            commentary: false,
            target_messages: false,
            queue_delivery: false,
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
        Some(AgentResponseHandling::new(
            /*commentary*/ true,
            AgentFinalResponseHandling::Wake,
            /*target_messages*/ true,
            /*queue_input*/ false,
        )),
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
            target_messages: true,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::Presentation,
        })
    );

    state.replace_final_response(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        AgentFinalResponseHandling::None,
    );

    assert_eq!(
        state.get(observer, target),
        Some(AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: true,
            target_messages: true,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::None,
        })
    );
}

#[test]
fn scoped_reply_route_is_visible_without_implying_final_delivery() {
    let observer = thread_id("00000000-0000-0000-0000-000000000101");
    let target = thread_id("00000000-0000-0000-0000-000000000102");
    let mut state = AgentResponseObservationState::default();
    state.note(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::None,
            /*target_messages*/ true,
            /*queue_input*/ false,
        )),
    );

    let observation = state
        .get(observer, target)
        .expect("scoped reply route should be visible");
    assert_eq!(
        observation,
        AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: false,
            target_messages: true,
            queue_delivery: false,
            final_response: AgentFinalResponseDisplay::None,
        }
    );
    assert_eq!(observation.compact_label(), "replies");
}

#[test]
fn queued_delivery_is_visible_after_target_turn_admission() {
    let observer = thread_id("00000000-0000-0000-0000-000000000101");
    let target = thread_id("00000000-0000-0000-0000-000000000102");
    let mut state = AgentResponseObservationState::default();
    state.note(
        observer,
        target,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::Passive,
            /*target_messages*/ false,
            /*queue_input*/ true,
        )),
    );

    let observation = state
        .get(observer, target)
        .expect("queued response should remain visible while target runs");
    assert_eq!(
        observation,
        AgentResponseObservationDisplay {
            binding: AgentResponseObservationBinding::Bound,
            commentary: false,
            target_messages: false,
            queue_delivery: true,
            final_response: AgentFinalResponseDisplay::Passive,
        }
    );
    assert_eq!(observation.compact_label(), "passive · queued");
}
