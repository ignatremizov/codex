//! Exact observer reservations remain independent even when they share a native terminal.

use super::*;

#[tokio::test]
async fn observation_failure_releases_only_its_exact_observer_and_shared_native_receipt() {
    let harness = AgentControlHarness::new().await;
    let (_, first) = harness.start_thread().await;
    let (_, second) = harness.start_thread().await;
    let first_control = &first.session.services.agent_control;
    let second_control = &second.session.services.agent_control;
    let first_id = first.session.presentation_id();
    let second_id = second.session.presentation_id();
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let _native = first_control
        .register_completion_watcher_with_parent(child, &first, "/root/child")
        .expect("native parent registration");
    let mut registrations = Vec::new();
    for (control, observer) in [(first_control, &first), (second_control, &second)] {
        registrations.push(
            control
                .register_response_watcher_with_parent_at_sequence(
                    child,
                    observer,
                    ResponseObservationPolicy::from_parts(
                        /*commentary*/ true,
                        FinalResponseObservation::Wake,
                    ),
                    /*retain_passive_completion_relationship*/ false,
                    /*target_turn_id*/ Some("actual-turn".to_owned()),
                    ResponseObservationBinding::NextTurn,
                    ResponseObservationPersistence::Durable,
                    /*minimum_event_sequence*/ 0,
                    /*after_item_id*/ None,
                    /*selection_id*/ None,
                )
                .expect("independent observation"),
        );
    }
    let status = AgentStatus::Completed(Some("the complete result".to_owned()));
    let first_terminal = first_control
        .record_response_observation_terminal(first_id, child, "actual-turn", status.clone())
        .expect("first accepted terminal");
    let second_terminal = second_control
        .record_response_observation_terminal(second_id, child, "actual-turn", status.clone())
        .expect("second accepted terminal");
    let native = first_control
        .record_agent_terminal_presentation(
            first_id,
            child,
            "actual-turn",
            status,
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("native route shares accepted terminal");
    assert_eq!(
        native.completion_context_response_item_id(),
        first_terminal.completion_context_response_item_id(),
    );
    assert_ne!(
        first_terminal.completion_context_response_item_id(),
        second_terminal.completion_context_response_item_id(),
    );
    first_control.abandon_response_observer(first_id, child, "ambiguous canonical receipt");
    assert!(first.session.submission_admission.check_ready().is_err());
    assert!(second.session.submission_admission.check_ready().is_ok());
    assert!(native.take_parent_thread().is_none());
    assert!(native.take_accepted_completion_delivery().is_none());
    assert!(
        first_control
            .take_response_observation_terminal(first_id, child)
            .is_none()
    );
    assert!(!first_control.has_future_response_observation(first_id, child));
    assert_eq!(
        first_control.completion_parent_for_child(child, first_id.thread_id),
        Some(first_id),
    );
    assert_eq!(
        second_control.completion_parent_for_child(child, second_id.thread_id),
        None,
    );
    // Revoking future policy does not erase an obligation already accepted by the other
    // observer. Its exact retained session and capability are still present.
    second_control.revoke_response_observations_for_child(child);
    assert!(second_terminal.take_parent_thread().is_some());
    assert!(
        second_terminal
            .take_accepted_completion_delivery()
            .is_some()
    );
    drop(registrations);
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn explicit_selection_can_downgrade_an_unclaimed_mailbox_wake() {
    let harness = AgentControlHarness::new().await;
    let (_, observer) = harness.start_thread().await;
    let control = &observer.session.services.agent_control;
    let parent = observer.session.presentation_id();
    for replacement in [
        FinalResponseObservation::None,
        FinalResponseObservation::PresentationOnly,
        FinalResponseObservation::Passive,
    ] {
        let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
        let selection = presentation::ResponseObservationSelection {
            selection_id: Uuid::now_v7(),
        };
        control.install_mailbox_final_subscription(parent, child, "mail-token");
        control.bind_mailbox_final_subscription_to_turn(parent, child, "mail-token", "bound-turn");
        let registration = control.register_response_watcher_with_parent_at_sequence(
            child,
            &observer,
            ResponseObservationPolicy::from_parts(/*commentary*/ false, replacement),
            /*retain_passive_completion_relationship*/ false,
            Some("bound-turn".into()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 0,
            /*after_item_id*/ None,
            Some(selection.selection_id),
        );
        control.retire_mailbox_final_subscription_preserving_observation(
            parent,
            child,
            "mail-token",
            &selection,
        );
        assert_eq!(
            (
                control.response_observation_turn_final_response(parent, child, "bound-turn"),
                control.mailbox_final_subscription_for_turn(parent, child, "bound-turn"),
                control.mailbox_final_subscription_message_id(parent, child),
            ),
            (Some(replacement), None, None),
        );
        drop(registration);
    }
    observer
        .shutdown_and_wait()
        .await
        .expect("shutdown observer");
}
