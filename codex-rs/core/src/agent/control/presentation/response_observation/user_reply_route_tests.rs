use super::*;
use pretty_assertions::assert_eq;

#[test]
fn route_patch_preserves_turn_work_and_does_not_publish_before_commit() {
    let control = LocalAgentControl::default();
    let parent = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let prepared = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Enabled)
        .expect("prepare route");
    assert_eq!(control.target_message_route_mode(parent, child), None);
    let turn = ResponseTurnObservation {
        final_response: FinalResponseObservation::Wake,
        task_preview: Some("accepted work".into()),
        ..Default::default()
    };
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .entry((parent, child))
        .or_default()
        .turns
        .insert("accepted-turn".into(), turn.clone());
    control
        .commit_reply_route(prepared)
        .expect("commit narrow route patch");
    assert!(
        control
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child[&(parent, child)]
            .turns
            .get("accepted-turn")
            == Some(&turn),
    );
}

#[test]
fn disabling_blocks_new_replies_but_retains_an_accepted_wake() {
    let control = LocalAgentControl::default();
    let parent = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let prepared = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Enabled)
        .expect("prepare enable");
    control.commit_reply_route(prepared).expect("enable");
    let TargetMessageAdmission::Wake(reservation) = control
        .target_message_admission(
            parent,
            child,
            "later-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SeparateTurn,
        )
        .expect("reserve accepted queued reply")
    else {
        panic!("expected wake");
    };
    let disabled = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Disabled)
        .expect("prepare disable");
    control.commit_reply_route(disabled).expect("disable");
    assert!(
        control
            .target_message_admission(
                parent,
                child,
                "another-turn",
                Some("active-source-turn"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err()
    );
    assert!(control.commit_target_message_wake(
        parent,
        child,
        "later-turn",
        reservation,
        "accepted-source-turn",
    ));
    control.finish_target_message_wake(parent, "accepted-source-turn");
    assert_eq!(
        control.target_message_route_mode(parent, child),
        Some(TargetMessageRouteMode::Disabled)
    );

    let enabled = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Enabled)
        .expect("prepare re-enable");
    control.commit_reply_route(enabled).expect("re-enable");
    assert!(matches!(
        control
            .target_message_admission(
                parent,
                child,
                "still-later-turn",
                /*observer_active_turn_id*/ None,
                Some("accepted-source-turn"),
                TargetMessageAdmissionMode::SeparateTurn,
            )
            .expect("later reply"),
        TargetMessageAdmission::Wake(_)
    ));
}

#[test]
fn revocation_prevents_stale_route_commit_and_new_instance_inheritance() {
    let control = LocalAgentControl::default();
    let parent = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let enabled = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Enabled)
        .expect("prepare");
    control.commit_reply_route(enabled).expect("enable");
    let stale = control
        .prepare_reply_route(parent, child, TargetMessageRouteMode::Disabled)
        .expect("prepare old patch");
    control.revoke_response_observations_for_child(child);
    assert!(control.commit_reply_route(stale).is_err());
    assert_eq!(control.target_message_route_mode(parent, child), None);
    let replacement = SessionPresentationId::new(child.thread_id, Uuid::now_v7());
    assert_eq!(control.target_message_route_mode(parent, replacement), None);
}
