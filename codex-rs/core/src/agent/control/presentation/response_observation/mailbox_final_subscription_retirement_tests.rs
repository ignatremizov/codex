use super::super::ResponseTurnObservation;
use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn endpoints() -> (AgentControl, SessionPresentationId, SessionPresentationId) {
    let observer_thread = ThreadId::new();
    let target_thread = ThreadId::new();
    (
        AgentControl::default(),
        SessionPresentationId::new(observer_thread, uuid::Uuid::nil()),
        SessionPresentationId::new(target_thread, uuid::Uuid::nil()),
    )
}

#[test]
fn explicit_next_turn_wake_suppresses_unclaimed_bound_mailbox_turn() {
    let (control, parent, child) = endpoints();
    let message_id = "bound-mailbox-turn";
    let selection_id = uuid::Uuid::new_v4();
    control.install_mailbox_final_subscription(parent, child, message_id);
    control.bind_mailbox_final_subscription_to_turn(parent, child, message_id, "turn-t");
    {
        let mut state = control.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        let pending = ResponseTurnObservation {
            final_response: FinalResponseObservation::Wake,
            response_observation_selection_id: Some(selection_id),
            ..ResponseTurnObservation::default()
        };
        relationship.pending_next_turn = Some(pending);
    }

    assert!(
        control.retire_mailbox_final_subscription_preserving_observation(
            parent,
            child,
            message_id,
            &ResponseObservationSelection { selection_id },
        )
    );

    let state = control.wait_agent_presentations.state();
    let relationship = state
        .response_observation_by_observer_child
        .get(&(parent, child))
        .expect("observation relationship");
    assert_eq!(
        relationship.turns["turn-t"].final_response,
        FinalResponseObservation::None,
        "an unclaimed token-owned prior final must not become an ordinary wake",
    );
    assert_eq!(
        relationship.turns["turn-t"].mailbox_final_subscription_message_id,
        None,
    );
    assert_eq!(
        relationship
            .pending_next_turn
            .as_ref()
            .map(|observation| observation.final_response),
        Some(FinalResponseObservation::Wake),
        "the exact explicit next-turn policy remains active",
    );
    assert_eq!(
        relationship
            .pending_next_turn
            .as_ref()
            .and_then(|observation| observation.response_observation_selection_id),
        None,
        "the transient selection identity is retired after preserving its policy",
    );
    assert_eq!(relationship.mailbox_final_subscription_message_id, None,);
    assert_eq!(
        relationship
            .mailbox_final_subscription_suppressed_message_id
            .as_deref(),
        Some(message_id),
    );
    drop(state);
}

#[test]
fn explicit_turn_selection_preserves_only_its_token_owned_final() {
    let (control, parent, child) = endpoints();
    let message_id = "bound-mailbox-turn";
    let selection_id = uuid::Uuid::new_v4();
    control.install_mailbox_final_subscription(parent, child, message_id);
    control.bind_mailbox_final_subscription_to_turn(parent, child, message_id, "turn-t");
    control.bind_mailbox_final_subscription_to_turn(parent, child, message_id, "turn-u");
    {
        let mut state = control.wait_agent_presentations.state();
        state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .expect("observation relationship")
            .turns
            .get_mut("turn-u")
            .expect("selected turn")
            .response_observation_selection_id = Some(selection_id);
    }

    assert!(
        control.retire_mailbox_final_subscription_preserving_observation(
            parent,
            child,
            message_id,
            &ResponseObservationSelection { selection_id },
        )
    );

    let state = control.wait_agent_presentations.state();
    let relationship = state
        .response_observation_by_observer_child
        .get(&(parent, child))
        .expect("observation relationship");
    assert_eq!(
        relationship.turns["turn-t"].final_response,
        FinalResponseObservation::None,
    );
    assert_eq!(
        relationship.turns["turn-u"].final_response,
        FinalResponseObservation::Wake,
    );
    assert_eq!(
        relationship.turns["turn-u"].response_observation_selection_id,
        None,
    );
}

#[test]
fn selected_next_turn_identity_survives_binding_before_mailbox_retirement() {
    let (control, parent, child) = endpoints();
    let message_id = "bound-mailbox-turn";
    let selection_id = uuid::Uuid::new_v4();
    control.install_mailbox_final_subscription(parent, child, message_id);
    control.bind_mailbox_final_subscription_to_turn(parent, child, message_id, "turn-t");
    {
        let mut state = control.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .expect("observation relationship");
        relationship.pending_next_turn = Some(ResponseTurnObservation {
            final_response: FinalResponseObservation::Wake,
            response_observation_selection_id: Some(selection_id),
            ..Default::default()
        });
    }

    control.bind_response_observation_turn(
        parent,
        child,
        "turn-u",
        ResponseObservationBinding::NextTurn,
    );

    assert!(
        control.retire_mailbox_final_subscription_preserving_observation(
            parent,
            child,
            message_id,
            &ResponseObservationSelection { selection_id },
        )
    );

    let state = control.wait_agent_presentations.state();
    let relationship = state
        .response_observation_by_observer_child
        .get(&(parent, child))
        .expect("observation relationship");
    assert_eq!(
        relationship.turns["turn-t"].final_response,
        FinalResponseObservation::None,
    );
    assert_eq!(
        relationship.turns["turn-u"].final_response,
        FinalResponseObservation::Wake,
    );
    assert_eq!(
        relationship.turns["turn-u"].mailbox_final_subscription_message_id,
        None,
    );
    assert_eq!(
        relationship.turns["turn-u"].response_observation_selection_id,
        None,
    );
}

#[test]
fn mailbox_retirement_preserves_an_already_claimed_final_receipt() {
    let (control, parent, child) = endpoints();
    let message_id = "claimed-mailbox-turn";
    control.install_mailbox_final_subscription(parent, child, message_id);
    control.bind_mailbox_final_subscription_to_turn(parent, child, message_id, "turn-t");
    let receipt_id = ResponseItemId::new("amsg");
    assert_eq!(
        control
            .prepare_final_response_observation_delivery(parent, child, "turn-t", &receipt_id)
            .1,
        Some(receipt_id.clone()),
    );

    assert!(
        control.retire_mailbox_final_subscription_preserving_observation(
            parent,
            child,
            message_id,
            &ResponseObservationSelection {
                selection_id: uuid::Uuid::new_v4(),
            },
        )
    );

    let state = control.wait_agent_presentations.state();
    let observation = &state
        .response_observation_by_observer_child
        .get(&(parent, child))
        .expect("observation relationship")
        .turns["turn-t"];
    assert_eq!(
        observation.final_delivery_response_item_id,
        Some(receipt_id),
    );
    assert_eq!(observation.final_response, FinalResponseObservation::Wake,);
}
