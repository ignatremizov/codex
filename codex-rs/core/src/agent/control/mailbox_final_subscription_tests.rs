use super::*;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
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

fn audit_observation(
    control: &AgentControl,
    observer: SessionPresentationId,
    target: SessionPresentationId,
    turn_id: Option<&str>,
) -> AgentResponseObservation {
    control
        .response_observation_audit_snapshots(observer, target, turn_id.map(ToOwned::to_owned))
        .into_iter()
        .find(|observation| observation.target_turn_id.as_deref() == turn_id)
        .expect("audit observation")
}

fn subscription(
    observer: SessionPresentationId,
    receiver: SessionPresentationId,
    message_id: &str,
    state: MailboxFinalSubscriptionState,
    bound_turn_id: Option<&str>,
) -> MailboxFinalSubscription {
    MailboxFinalSubscription {
        message_id: message_id.to_string(),
        receiver_thread_id: receiver.thread_id,
        sender_thread_id: observer.thread_id,
        acceptance_sequence: 1,
        state,
        bound_turn_id: bound_turn_id.map(ToOwned::to_owned),
        receiver_lifecycle_epoch: 0,
        sender_lifecycle_epoch: 0,
    }
}

#[test]
fn pending_subscription_suppresses_prior_wake_without_binding_a_turn() {
    let (control, observer, receiver) = endpoints();
    let mut prior = audit_observation(&control, observer, receiver, None);
    prior.baseline_final_delivery = AgentResponseFinalDelivery::Wake;
    prior.final_delivery = AgentResponseFinalDelivery::Wake;
    prior.mailbox_final_subscription_message_id = Some("older-token".to_string());
    let pending = subscription(
        observer,
        receiver,
        "pending-token",
        MailboxFinalSubscriptionState::Pending,
        None,
    );
    let effective = control.overlay_mailbox_final_subscription_observations(
        observer,
        receiver,
        vec![prior.clone()],
        Some(&pending),
        &HashSet::new(),
        &HashSet::new(),
    );

    prior.baseline_final_delivery = AgentResponseFinalDelivery::None;
    prior.final_delivery = AgentResponseFinalDelivery::None;
    prior.mailbox_final_subscription_message_id = Some("pending-token".to_string());
    prior.mailbox_final_subscription_suppressed_message_id = None;
    assert_eq!(effective, vec![prior]);
}

#[test]
fn bound_subscription_wakes_only_its_exact_turn_and_preserves_admitted_receipts() {
    let (control, observer, receiver) = endpoints();
    let mut root = audit_observation(&control, observer, receiver, None);
    root.baseline_final_delivery = AgentResponseFinalDelivery::Wake;
    root.final_delivery = AgentResponseFinalDelivery::Wake;
    let presentation_turn =
        audit_observation(&control, observer, receiver, Some("presentation-turn"));
    let mut admitted_turn = audit_observation(&control, observer, receiver, Some("admitted-turn"));
    admitted_turn.final_delivery = AgentResponseFinalDelivery::Wake;
    admitted_turn.final_delivery_response_item_id =
        Some(ResponseItemId::new("already-delivered-final"));
    let mut bound_turn = audit_observation(&control, observer, receiver, Some("bound-turn"));
    let bound = subscription(
        observer,
        receiver,
        "bound-token",
        MailboxFinalSubscriptionState::Bound,
        Some("bound-turn"),
    );
    let effective = control.overlay_mailbox_final_subscription_observations(
        observer,
        receiver,
        vec![
            root.clone(),
            presentation_turn.clone(),
            admitted_turn.clone(),
            bound_turn.clone(),
        ],
        Some(&bound),
        &HashSet::new(),
        &HashSet::new(),
    );

    root.baseline_final_delivery = AgentResponseFinalDelivery::None;
    root.final_delivery = AgentResponseFinalDelivery::None;
    root.mailbox_final_subscription_message_id = Some("bound-token".to_string());
    bound_turn.final_delivery = AgentResponseFinalDelivery::Wake;
    bound_turn.mailbox_final_subscription_message_id = Some("bound-token".to_string());
    assert_eq!(
        effective,
        vec![root, presentation_turn, admitted_turn, bound_turn],
    );
}

#[test]
fn stale_token_tombstones_only_its_unadmitted_wake() {
    let (control, observer, receiver) = endpoints();
    let mut root = audit_observation(&control, observer, receiver, None);
    root.final_delivery = AgentResponseFinalDelivery::Wake;
    root.mailbox_final_subscription_message_id = Some("stale-token".to_string());
    let mut token_turn = audit_observation(&control, observer, receiver, Some("token-turn"));
    token_turn.final_delivery = AgentResponseFinalDelivery::Wake;
    token_turn.mailbox_final_subscription_message_id = Some("stale-token".to_string());
    let ordinary_turn = audit_observation(&control, observer, receiver, Some("ordinary-turn"));
    let mut admitted_turn = audit_observation(&control, observer, receiver, Some("admitted-turn"));
    admitted_turn.final_delivery = AgentResponseFinalDelivery::Wake;
    admitted_turn.final_delivery_response_item_id = Some(ResponseItemId::new("committed-final"));
    admitted_turn.mailbox_final_subscription_message_id = Some("stale-token".to_string());

    let effective = control.overlay_mailbox_final_subscription_observations(
        observer,
        receiver,
        vec![
            root.clone(),
            token_turn.clone(),
            ordinary_turn.clone(),
            admitted_turn.clone(),
        ],
        None,
        &HashSet::from(["stale-token".to_string()]),
        &HashSet::new(),
    );

    root.final_delivery = AgentResponseFinalDelivery::None;
    root.mailbox_final_subscription_message_id = None;
    root.mailbox_final_subscription_suppressed_message_id = Some("stale-token".to_string());
    token_turn.final_delivery = AgentResponseFinalDelivery::None;
    token_turn.mailbox_final_subscription_message_id = None;
    assert_eq!(
        effective,
        vec![root, token_turn, ordinary_turn, admitted_turn],
    );
}
