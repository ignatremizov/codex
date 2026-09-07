use super::*;
use pretty_assertions::assert_eq;

#[test]
fn continuity_cannot_bridge_a_different_published_presentation() {
    let thread_id = ThreadId::new();
    let source = SessionPresentationId::new(thread_id, Uuid::from_u128(1));
    let replacement = SessionPresentationId::new(thread_id, Uuid::from_u128(2));
    // Absence from public discovery also covers an including-pending replacement.
    // Publication, not presence in the pending registry, ends that continuity.
    let cases = [
        (None, 7, true),
        (Some(source), 7, true),
        (Some(replacement), 7, false),
        (None, 8, false),
        (Some(source), 8, false),
        (Some(replacement), 8, false),
    ];
    let actual = cases.map(|(published, current_generation, _)| {
        messaging_continuity_is_current(
            source,
            /*generation*/ 7,
            current_generation,
            published,
        )
    });
    assert_eq!(actual, cases.map(|(_, _, expected)| expected));
}

#[test]
fn queued_message_reservation_requires_the_effective_route_at_admission() {
    let control = AgentControl::default();
    let recipient = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let sender = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Enabled);
    let super::super::TargetMessageAdmission::Wake(reservation) = control
        .target_message_admission(
            recipient,
            sender,
            "sender-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            super::super::TargetMessageAdmissionMode::SeparateTurn,
        )
        .expect("enabled route reserves a queued turn")
    else {
        panic!("expected wake reservation");
    };
    assert!(control.target_message_wake_is_current(recipient, sender, "sender-turn", reservation));
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Disabled);
    assert!(!control.target_message_wake_is_current(recipient, sender, "sender-turn", reservation));
}

#[test]
fn nearest_common_supervisor_controls_existing_and_future_members() {
    let root = ThreadId::new();
    let supervisor = ThreadId::new();
    let frontend = ThreadId::new();
    let backend = ThreadId::new();
    let reviewer = ThreadId::new();
    let outside = ThreadId::new();
    let policy_id = SessionPresentationId::new(supervisor, Uuid::now_v7());
    let mut parents = HashMap::from([
        (supervisor, root),
        (frontend, supervisor),
        (backend, supervisor),
        (outside, root),
    ]);
    let mut policies = HashMap::from([(policy_id, (0, TargetMessageRouteMode::Enabled))]);
    assert_eq!(
        subtree_mode(frontend, backend, &parents, &policies),
        Some(TargetMessageRouteMode::Enabled)
    );
    assert_eq!(
        subtree_mode(backend, frontend, &parents, &policies),
        Some(TargetMessageRouteMode::Enabled)
    );
    assert_eq!(subtree_mode(frontend, root, &parents, &policies), None);
    assert_eq!(subtree_mode(frontend, outside, &parents, &policies), None);
    assert_eq!(
        subtree_mode(supervisor, frontend, &parents, &policies),
        None,
        "subtree policy does not gate downward task dispatch"
    );
    parents.insert(reviewer, backend);
    assert_eq!(
        subtree_mode(reviewer, frontend, &parents, &policies),
        Some(TargetMessageRouteMode::Enabled)
    );
    policies.insert(
        SessionPresentationId::new(backend, Uuid::now_v7()),
        (0, TargetMessageRouteMode::Disabled),
    );
    assert_eq!(
        subtree_mode(reviewer, backend, &parents, &policies),
        Some(TargetMessageRouteMode::Disabled)
    );
    parents.insert(reviewer, outside);
    assert_eq!(subtree_mode(reviewer, frontend, &parents, &policies), None);
}

#[test]
fn explicit_pair_mode_overrides_inherited_mode_in_admission() {
    let control = AgentControl::default();
    let recipient = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let sender = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Enabled);
    let prepared = control.prepare_target_message_route_replacement(
        recipient,
        sender,
        TargetMessageRouteMode::Disabled,
    );
    assert!(control.commit_target_message_route_replacement(recipient, sender, &prepared));
    assert_eq!(
        control.target_message_route_mode(recipient, sender),
        Some(TargetMessageRouteMode::Disabled)
    );
    assert!(
        control
            .target_message_admission(
                recipient,
                sender,
                "turn",
                Some("active"),
                /*observer_last_terminal_turn_id*/ None,
                super::super::TargetMessageAdmissionMode::SteerOrWake
            )
            .is_err()
    );
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Disabled);
    let prepared = control.prepare_target_message_route_replacement(
        recipient,
        sender,
        TargetMessageRouteMode::Enabled,
    );
    assert!(control.commit_target_message_route_replacement(recipient, sender, &prepared));
    assert!(matches!(
        control.target_message_admission(
            recipient,
            sender,
            "turn",
            Some("active"),
            /*observer_last_terminal_turn_id*/ None,
            super::super::TargetMessageAdmissionMode::SteerOrWake
        ),
        Ok(super::super::TargetMessageAdmission::Steer)
    ));
}
