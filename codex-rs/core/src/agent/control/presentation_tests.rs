use super::*;
use crate::agent::control::response_observer::CompletionWatcherLifecycleGuard;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::session::TurnInput;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;
use std::time::Duration;

fn completed_status() -> AgentStatus {
    AgentStatus::Completed(Some("done".to_string()))
}

fn session_presentation_id(thread_id: ThreadId) -> SessionPresentationId {
    SessionPresentationId::new(
        thread_id,
        uuid::Uuid::parse_str(&thread_id.to_string()).expect("thread UUID"),
    )
}

#[test]
fn target_message_grant_allows_one_idle_wake_and_only_steers_that_wake_turn() {
    let control = AgentControl::default();
    let observer = session_presentation_id(ThreadId::new());
    let target = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    assert!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                Some("already-active"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err(),
        "knowing a same-root target is not enough to send without an exact-turn grant"
    );
    let registration = control
        .register_response_watcher_with_admission(
            target,
            observer,
            &admission,
            ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
                /*target_messages*/ true,
                /*queue_input*/ false,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("target-turn".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("target-message watcher");

    assert_eq!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                Some("already-active"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("active observer should accept a steer"),
        TargetMessageAdmission::Steer
    );
    let TargetMessageAdmission::Wake(reservation_id) = control
        .target_message_admission(
            observer,
            target,
            "target-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SteerOrWake,
        )
        .expect("idle observer should reserve its one wake")
    else {
        panic!("idle observer should reserve a wake");
    };
    assert_eq!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                /*observer_active_turn_id*/ None,
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("concurrent sender should observe the pending reservation"),
        TargetMessageAdmission::PendingWake
    );
    assert_eq!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                Some("already-active"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("a reserved future wake must not block an active-turn steer"),
        TargetMessageAdmission::Steer
    );
    assert!(control.commit_target_message_wake(
        observer,
        target,
        "target-turn",
        reservation_id,
        "message-wake-turn",
    ));
    assert_eq!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                Some("message-wake-turn"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("later messages should steer the same wake turn"),
        TargetMessageAdmission::Steer
    );
    assert!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                /*observer_active_turn_id*/ None,
                Some("message-wake-turn"),
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err(),
        "the grant must not start another turn after its wake completes"
    );

    control.finish_target_message_wake(observer, "message-wake-turn");
    assert!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                /*observer_active_turn_id*/ None,
                Some("message-wake-turn"),
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err(),
        "finishing the wake should revoke the live reply route"
    );

    drop(registration);
    let _renewed_registration = control
        .register_response_watcher_with_admission(
            target,
            observer,
            &admission,
            ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
                /*target_messages*/ true,
                /*queue_input*/ false,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("target-turn".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("renewed target-message watcher");
    assert!(matches!(
        control
            .target_message_admission(
                observer,
                target,
                "target-turn",
                /*observer_active_turn_id*/ None,
                Some("message-wake-turn"),
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("a new explicit m dispatch should renew the route"),
        TargetMessageAdmission::Wake(_)
    ));
}

#[tokio::test]
async fn active_targeted_wait_commits_terminal_presentation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn targeted_wait_claims_a_watcher_terminal_queued_before_registration() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child.thread_id, completed_status())])
    );
    commit.commit();
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("queued watcher terminal");

    assert!(terminal.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn any_child_wait_claims_a_watcher_terminal_queued_before_registration() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    let wait = control.register_any_child_wait_agent_presentation(parent);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child.thread_id, completed_status())])
    );
    commit.commit();
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("queued watcher terminal");

    assert!(terminal.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn targeted_wait_claims_an_in_flight_watcher_terminal() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("watcher should take the queued terminal");

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child.thread_id, completed_status())])
    );
    commit.commit();

    assert!(terminal.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_wait_claims_only_the_observed_turn_from_multiple_pending_terminals() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let first = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("first watcher terminal should be in flight");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-2",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_terminal_statuses(&HashMap::from([(
        child.thread_id,
        (Some("turn-2".to_string()), completed_status()),
    )]));
    assert_eq!(
        commit
            .claimed_target_turns()
            .into_iter()
            .map(|claimed| (claimed.child, claimed.turn_id))
            .collect::<Vec<_>>(),
        vec![(child, "turn-2".to_string())]
    );
    commit.commit();
    let second = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("second watcher terminal should remain queued");

    assert!(!first.presentation.wait_owns_presentation().await);
    assert!(second.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn delayed_historical_terminal_is_deduplicated_after_a_newer_turn() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let first = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("first turn should be in flight");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-2",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    assert!(
        control
            .record_agent_terminal_presentation(
                parent,
                child,
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Watcher,
                || {},
            )
            .is_none(),
        "the delayed durable event must not recreate turn 1"
    );
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_terminal_statuses(&HashMap::from([(
        child.thread_id,
        (Some("turn-1".to_string()), completed_status()),
    )]));
    assert_eq!(
        commit
            .claimed_target_turns()
            .into_iter()
            .map(|claimed| (claimed.child, claimed.turn_id))
            .collect::<Vec<_>>(),
        vec![(child, "turn-1".to_string())]
    );
    commit.commit();
    let second = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("second turn should remain queued");

    assert!(first.presentation.wait_owns_presentation().await);
    assert!(!second.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn exact_turn_wait_claims_every_reconstructed_copy() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let first = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("first turn should be in flight");
    control
        .wait_agent_presentations
        .state()
        .terminal_turns_by_observer_child
        .remove(&(parent, child));
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_terminal_statuses(&HashMap::from([(
        child.thread_id,
        (Some("turn-1".to_string()), completed_status()),
    )]));
    assert_eq!(commit.claimed_target_turns().len(), 1);
    commit.commit();
    let reconstructed = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("reconstructed turn should remain queued");

    assert!(first.presentation.wait_owns_presentation().await);
    assert!(reconstructed.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn exact_turn_wait_persists_one_target_across_runtime_presentations() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let reloaded_child = SessionPresentationId::new(child.thread_id, uuid::Uuid::now_v7());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let first = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("first runtime presentation should be in flight");
    let _ = control.record_agent_terminal_presentation(
        parent,
        reloaded_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_terminal_statuses(&HashMap::from([(
        child.thread_id,
        (Some("turn-1".to_string()), completed_status()),
    )]));
    assert_eq!(
        commit
            .claimed_target_turns()
            .into_iter()
            .map(|claimed| (claimed.child, claimed.turn_id))
            .collect::<Vec<_>>(),
        vec![(reloaded_child, "turn-1".to_string())]
    );
    commit.commit();
    let reloaded = control
        .take_watcher_terminal_presentation(parent, reloaded_child)
        .expect("reloaded runtime presentation should remain queued");

    assert!(first.presentation.wait_owns_presentation().await);
    assert!(reloaded.presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn automatic_delivery_claim_prevents_a_late_wait_from_suppressing_it() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("watcher should take the queued terminal");
    assert!(!terminal.presentation.wait_owns_presentation().await);

    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(commit.agent_states(), HashMap::new());
    commit.commit();
}

#[tokio::test]
async fn wait_registration_cannot_cross_terminal_publication() {
    let control = Arc::new(AgentControl::default());
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
    let (registration_started_tx, registration_started_rx) = std::sync::mpsc::sync_channel(1);
    let record_control = Arc::clone(&control);
    let recorder = std::thread::spawn(move || {
        record_control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {
                    published_tx.send(()).expect("publication signal");
                    registration_started_rx
                        .recv()
                        .expect("registration start signal");
                },
            )
            .expect("direct presentation")
    });
    published_rx.recv().expect("terminal should publish");
    let wait_control = Arc::clone(&control);
    let waiter = std::thread::spawn(move || {
        registration_started_tx
            .send(())
            .expect("registration start signal");
        wait_control.register_targeted_wait_agent_presentation(parent, &[child_thread_id])
    });

    let presentation = recorder.join().expect("recorder should finish");
    waiter
        .join()
        .expect("wait registration should finish")
        .freeze_for_children([child_thread_id])
        .commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_targeted_wait_does_not_claim_previous_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_mailbox_wait_reports_previous_terminal_without_claiming_its_presentation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    control.authorize_pending_completion_context(parent, &presentation);
    let response_item_ids = vec![presentation.completion_context_response_item_id()];
    let wait = control.register_any_child_wait_agent_presentation(parent);

    let commit = wait.freeze_for_mailbox_response_item_ids(&response_item_ids);

    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    commit.commit();
    assert!(!presentation.wait_owns_presentation().await);
}

#[test]
fn cancelled_late_mailbox_wait_preserves_pending_terminal_state() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    control.authorize_pending_completion_context(parent, &presentation);
    let response_item_ids = vec![presentation.completion_context_response_item_id()];
    let cancelled_wait = control.register_any_child_wait_agent_presentation(parent);
    let cancelled_commit = cancelled_wait.freeze_for_mailbox_response_item_ids(&response_item_ids);
    drop(cancelled_commit);
    let next_wait = control.register_any_child_wait_agent_presentation(parent);

    assert_eq!(
        next_wait
            .freeze_for_mailbox_response_item_ids(&response_item_ids)
            .agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
}

#[test]
fn mailbox_wait_uses_latest_queued_generation_for_a_child() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let first = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("first presentation");
    let latest_status = AgentStatus::Errored("latest failed".to_string());
    let latest = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-2",
            latest_status.clone(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("latest presentation");
    control.authorize_pending_completion_context(parent, &first);
    control.authorize_pending_completion_context(parent, &latest);
    let response_item_ids = vec![
        first.completion_context_response_item_id(),
        latest.completion_context_response_item_id(),
    ];
    let wait = control.register_any_child_wait_agent_presentation(parent);

    assert_eq!(
        wait.freeze_for_mailbox_response_item_ids(&response_item_ids)
            .agent_states(),
        HashMap::from([(child_thread_id, latest_status)])
    );
}

#[tokio::test]
async fn targeted_wait_after_previous_terminal_can_claim_next_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let previous = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("previous presentation");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let next = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("next presentation");

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(!previous.wait_owns_presentation().await);
    assert!(next.wait_owns_presentation().await);
}

#[tokio::test]
async fn cancelled_wait_releases_terminal_to_background() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");

    drop(wait);

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn frozen_wait_does_not_claim_later_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let first_child_thread_id = ThreadId::new();
    let second_child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(
        parent,
        &[first_child_thread_id, second_child_thread_id],
    );
    let first = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(first_child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("first presentation");
    let commit = wait.freeze_for_children([first_child_thread_id]);
    let second = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(second_child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("second presentation");

    commit.commit();

    assert!(first.wait_owns_presentation().await);
    assert!(!second.wait_owns_presentation().await);
}

#[tokio::test]
async fn any_child_wait_claims_only_terminal_messages_already_in_the_mailbox() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let delivered_child_thread_id = ThreadId::new();
    let blocked_child_thread_id = ThreadId::new();
    let wait = control.register_any_child_wait_agent_presentation(parent);
    let delivered = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(delivered_child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let blocked = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(blocked_child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let delivered_response_item_ids = vec![delivered.completion_context_response_item_id()];

    let commit = wait.freeze_for_mailbox_response_item_ids(&delivered_response_item_ids);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(delivered_child_thread_id, completed_status())])
    );
    commit.commit();

    assert!(delivered.wait_owns_presentation().await);
    assert!(!blocked.wait_owns_presentation().await);
}

#[test]
fn finishing_old_watcher_terminal_preserves_new_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-old",
        AgentStatus::Shutdown,
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let old = control
        .take_watcher_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
        )
        .expect("old watcher terminal");
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-new",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    control.finish_watcher_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        &old.turn_id,
    );

    let new = control
        .take_watcher_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
        )
        .expect("new watcher terminal");
    assert_eq!(new.turn_id, "turn-new");
    assert_eq!(new.status, completed_status());
}

#[test]
fn requeued_watcher_terminal_preserves_its_presentation() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_completion_watcher_with_admission(child, parent, &admission)
        .expect("watcher registration");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("queued terminal");
    let response_item_id = terminal.presentation.completion_context_response_item_id();

    control.requeue_watcher_terminal_presentation(parent, child, terminal);

    let requeued = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("requeued terminal");
    assert_eq!(requeued.turn_id, "turn-1");
    assert_eq!(
        requeued.presentation.completion_context_response_item_id(),
        response_item_id
    );
    assert!(requeued.presentation.has_accepted_completion_delivery());
}

#[test]
fn completion_context_response_item_ids_require_authorization() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(ThreadId::new()),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();

    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    control.authorize_pending_completion_context(parent, &presentation);
    let unrelated_parent_thread_id = ThreadId::new();
    let unrelated_parent = session_presentation_id(unrelated_parent_thread_id);
    let unrelated_wait = control.register_any_child_wait_agent_presentation(unrelated_parent);
    assert!(
        unrelated_wait
            .freeze_for_mailbox_response_item_ids(std::slice::from_ref(&id))
            .agent_states()
            .is_empty()
    );
    assert!(!control.claim_completion_context_response_item_id(unrelated_parent, &id));
    assert!(control.claim_completion_context_response_item_id(parent, &id));
    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    let wait = control
        .register_any_child_wait_agent_presentation(session_presentation_id(ThreadId::new()));
    assert!(
        wait.freeze_for_mailbox_response_item_ids(&[id])
            .agent_states()
            .is_empty()
    );
}

#[test]
fn claimed_completion_context_remains_available_to_a_later_wait() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let response_item_id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(parent, &presentation);

    assert!(control.claim_completion_context_response_item_id(parent, &response_item_id));
    let commit = control
        .register_any_child_wait_agent_presentation(parent)
        .freeze_for_mailbox_response_item_ids(&[response_item_id]);

    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    commit.commit();
}

#[test]
fn removing_parent_revokes_unclaimed_completion_context() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(parent, &presentation);

    control.clear_completion_contexts_for_session(parent);

    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    let wait = control.register_any_child_wait_agent_presentation(parent);
    assert!(
        wait.freeze_for_mailbox_response_item_ids(&[id])
            .agent_states()
            .is_empty()
    );
}

#[test]
fn closing_child_revokes_its_unclaimed_completion_context() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(parent, &presentation);

    let _ = control.revoke_response_observations_for_child(child_thread_id);

    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    assert!(
        control
            .register_any_child_wait_agent_presentation(parent)
            .freeze_for_mailbox_response_item_ids(&[id])
            .agent_states()
            .is_empty()
    );
}

#[test]
fn old_session_cleanup_preserves_new_session_authorization() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let presentation = control
        .record_agent_terminal_presentation(
            new_parent,
            session_presentation_id(ThreadId::new()),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(new_parent, &presentation);

    control.clear_completion_contexts_for_session(old_parent);

    assert_eq!(
        control
            .register_any_child_wait_agent_presentation(new_parent)
            .freeze_for_mailbox_response_item_ids(std::slice::from_ref(&id))
            .agent_states(),
        HashMap::from([(presentation.inner.child.thread_id, completed_status())])
    );
    assert!(control.claim_completion_context_response_item_id(new_parent, &id));
}

#[tokio::test]
async fn removed_parent_session_wait_cannot_claim_new_parent_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let old_wait =
        control.register_targeted_wait_agent_presentation(old_parent, &[child_thread_id]);

    control.clear_wait_agent_presentations_for_session(old_parent);
    let stale_wait =
        control.register_targeted_wait_agent_presentation(old_parent, &[child_thread_id]);
    let new_wait =
        control.register_targeted_wait_agent_presentation(new_parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            new_parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let old_commit = old_wait.freeze_for_children([child_thread_id]);
    let stale_commit = stale_wait.freeze_for_children([child_thread_id]);
    let new_commit = new_wait.freeze_for_children([child_thread_id]);

    assert!(old_commit.agent_states().is_empty());
    assert!(stale_commit.agent_states().is_empty());
    assert_eq!(
        new_commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    old_commit.commit();
    stale_commit.commit();
    new_commit.commit();
    assert!(presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn new_parent_session_wait_cannot_claim_old_parent_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(new_parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            old_parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let commit = wait.freeze_for_children([child_thread_id]);

    assert!(commit.agent_states().is_empty());
    commit.commit();
    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn removing_parent_session_invalidates_a_frozen_wait_commit() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let commit = wait.freeze_for_children([child_thread_id]);

    control.clear_wait_agent_presentations_for_session(parent);
    commit.commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[test]
fn completion_watcher_registration_is_scoped_to_session_instance() {
    let control = AgentControl::default();
    let thread_id = ThreadId::new();
    let parent = session_presentation_id(ThreadId::new());
    let first_session = SessionPresentationId::new(thread_id, uuid::Uuid::now_v7());
    let reloaded_session = SessionPresentationId::new(thread_id, uuid::Uuid::now_v7());
    let first_registration = control
        .register_completion_watcher(first_session, parent)
        .expect("first registration");

    assert!(
        control
            .register_completion_watcher(first_session, parent)
            .is_none()
    );
    assert!(
        control
            .register_completion_watcher(reloaded_session, parent)
            .is_some()
    );
    drop(first_registration);
    assert!(
        control
            .register_completion_watcher(first_session, parent)
            .is_some()
    );
}

#[test]
fn terminal_publication_reserves_registered_parent_delivery() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(crate::session::SubmissionAdmission::default());
    let _watcher_registration = control
        .register_completion_watcher_with_admission(child, parent, &admission)
        .expect("completion watcher registration");
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            child,
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("terminal presentation");

    let accepted_completion_delivery = presentation
        .take_accepted_completion_delivery()
        .expect("accepted completion delivery");
    assert!(presentation.take_accepted_completion_delivery().is_none());
    presentation.restore_accepted_completion_delivery(accepted_completion_delivery);
    assert!(presentation.take_accepted_completion_delivery().is_some());
    assert!(presentation.take_accepted_completion_delivery().is_none());
}

#[test]
fn old_child_session_cannot_consume_reloaded_child_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let old_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let reloaded_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        reloaded_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    assert!(
        control
            .take_watcher_terminal_presentation(
                session_presentation_id(parent_thread_id),
                old_child
            )
            .is_none()
    );
    assert!(
        control
            .take_watcher_terminal_presentation(
                session_presentation_id(parent_thread_id),
                reloaded_child
            )
            .is_some()
    );
}

#[test]
fn closing_child_clears_terminal_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );

    control.release_spawned_thread(SpawnedThreadRelease::Session(session_presentation_id(
        child_thread_id,
    )));

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_some()
    );
}

#[test]
fn releasing_old_child_session_preserves_new_terminal_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let old_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let new_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        old_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        new_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );

    control.release_spawned_thread(SpawnedThreadRelease::Session(old_child));

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                old_child,
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_some()
    );
    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                new_child,
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_none()
    );
}

#[test]
fn one_child_turn_queues_only_one_delivery_path() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-1",
        AgentStatus::Errored("failed".to_string()),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_none()
    );
    let terminal = control
        .take_watcher_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
        )
        .expect("watcher terminal");
    assert_eq!(terminal.status, AgentStatus::Errored("failed".to_string()));
    assert!(
        control
            .take_watcher_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id)
            )
            .is_none()
    );
}

#[test]
fn response_observation_merges_monotonically_for_one_target_turn() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");

    assert!(
        control
            .register_response_watcher_with_admission(
                child,
                parent,
                &admission,
                ResponseObservationPolicy::from_parts(
                    /*commentary*/ true,
                    FinalResponseObservation::None,
                ),
                /*retain_passive_completion_relationship*/ false,
                Some("turn-1".to_string()),
                ResponseObservationBinding::NextTurn,
                ResponseObservationPersistence::Durable,
            )
            .is_none()
    );

    let snapshots = control.response_observation_snapshots(parent, child);
    let turn = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("bound turn snapshot");
    assert!(turn.pending_commentary);
    assert_eq!(
        turn.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::Wake
    );
}

#[test]
fn explicit_user_observation_replaces_final_policy_without_removing_commentary() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");

    assert_eq!(
        control.replace_final_response_observation(
            parent,
            child,
            Some("turn-1"),
            /*last_terminal_turn_id*/ None,
            FinalResponseObservation::PresentationOnly,
        ),
        FinalResponseObservationReplacement::Replaced {
            previous: FinalResponseObservation::Wake,
            binding: ReplacedFinalResponseObservationBinding::ActiveTurn,
            task_preview: None,
        }
    );

    let snapshot = control
        .response_observation_snapshots(parent, child)
        .into_iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("turn observation");
    assert!(snapshot.pending_commentary);
    assert_eq!(
        snapshot.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly
    );
}

#[test]
fn prepared_user_observation_does_not_change_live_policy_before_commit() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::PresentationOnly,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let before = control.response_observation_snapshots(parent, child);
    let prepared = control
        .prepare_final_response_observation_replacement(
            parent,
            child,
            Some("turn-1"),
            /*last_terminal_turn_id*/ None,
            FinalResponseObservation::Wake,
        )
        .expect("prepare replacement");

    assert_eq!(
        control.response_observation_snapshots(parent, child),
        before
    );
    let prepared_snapshots =
        control.prepared_response_observation_replacement_snapshots(parent, child, &prepared);
    assert_eq!(
        prepared_snapshots
            .iter()
            .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
            .expect("prepared turn")
            .final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::Wake
    );
    assert!(
        control.commit_final_response_observation_replacement(parent, child, &prepared),
        "unchanged live state should accept the durable replacement"
    );
    assert_eq!(
        control.response_observation_snapshots(parent, child),
        prepared_snapshots
    );
}

#[test]
fn explicit_user_observation_promotes_hidden_task_preview_from_the_replaced_turn() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::PresentationOnly,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let task_preview = "review ".to_string() + &"the older change ".repeat(20);
    control.set_response_observation_task_preview(
        parent,
        child,
        /*target_turn_id*/ None,
        task_preview,
    );
    control.bind_response_observation_turn_at_sequence(ResponseObservationTurnBinding {
        parent,
        child,
        turn_id: "older-turn",
        binding: ResponseObservationBinding::NextTurn,
        commentary_boundary: None,
        task_preview: None,
        publication: ResponseObservationBindingPublication::Immediate,
    });

    assert_eq!(
        control.replace_final_response_observation(
            parent,
            child,
            Some("newer-turn"),
            Some("older-turn"),
            FinalResponseObservation::Passive,
        ),
        FinalResponseObservationReplacement::Replaced {
            previous: FinalResponseObservation::PresentationOnly,
            binding: ReplacedFinalResponseObservationBinding::UndeliveredCompletion,
            task_preview: Some(
                "review ".to_string() + &"the older change ".repeat(13) + "the older c…",
            ),
        }
    );
}

#[test]
fn explicit_user_observation_cannot_replace_claimed_final_delivery() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let response_item_id = codex_protocol::ResponseItemId::new("msg");
    assert_eq!(
        control.prepare_final_response_observation_delivery(
            parent,
            child,
            "turn-1",
            &response_item_id,
        ),
        (
            FinalResponseObservation::Wake,
            Some(response_item_id.clone()),
            false,
        )
    );

    assert_eq!(
        control.replace_final_response_observation(
            parent,
            child,
            Some("turn-1"),
            /*last_terminal_turn_id*/ None,
            FinalResponseObservation::PresentationOnly,
        ),
        FinalResponseObservationReplacement::DeliveryClaimed
    );
}

#[test]
fn fire_and_forget_audit_snapshot_preserves_an_existing_final_wake() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");

    let snapshots =
        control.response_observation_audit_snapshots(parent, child, Some("turn-1".to_string()));

    let turn = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("audited turn snapshot");
    assert_eq!(
        turn.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::Wake
    );
}

#[test]
fn only_bound_final_wakes_defer_automatic_idle_work() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");

    assert!(!control.has_bound_final_response_wake(parent));

    control.bind_response_observation_turn(
        parent,
        child,
        "turn-1",
        ResponseObservationBinding::NextTurn,
    );

    assert!(control.has_bound_final_response_wake(parent));

    let _ = control.finish_response_observation_turn(parent, child, "turn-1");

    assert!(!control.has_bound_final_response_wake(parent));

    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        ResponseObservationPolicy::from_parts(
            /*commentary*/ false,
            FinalResponseObservation::Wake,
        ),
        /*retain_passive_completion_relationship*/ false,
        Some("turn-2".to_string()),
        ResponseObservationBinding::NextTurn,
        ResponseObservationPersistence::Durable,
    );

    assert!(control.has_bound_final_response_wake(parent));
    assert!(control.revoke_response_observation_for_presentation(parent, child));
    assert!(!control.has_bound_final_response_wake(parent));

    let _replacement_registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-3".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("replacement watcher registration");

    assert!(control.has_bound_final_response_wake(parent));
    assert_eq!(
        control.revoke_response_observations_for_child(child.thread_id),
        vec![parent]
    );
    assert!(!control.has_bound_final_response_wake(parent));
}

#[test]
fn stale_setup_cleanup_cannot_revoke_a_replacement_response_watcher() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let mut original_registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("original watcher registration");
    let original_registration_id = control
        .response_watcher_registration_id(parent, child)
        .expect("original watcher identity");
    assert!(matches!(
        control.revoke_response_observation_if_registration_is_current(
            parent,
            child,
            original_registration_id,
        ),
        ConditionalResponseObservationRevocation::Revoked { .. }
    ));
    let _replacement_registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("replacement watcher registration");
    assert_ne!(
        control.response_watcher_registration_id(parent, child),
        Some(original_registration_id)
    );

    assert!(matches!(
        control.revoke_response_observation_if_registration_is_current(
            parent,
            child,
            original_registration_id,
        ),
        ConditionalResponseObservationRevocation::Replaced
    ));
    assert!(original_registration.retire_if_observation_idle());
    assert!(control.has_completion_watcher(parent, child));
    assert!(
        !control
            .response_observation_snapshots(parent, child)
            .is_empty()
    );
}

#[tokio::test]
async fn idle_turn_reservation_rechecks_a_wake_bound_after_idle_detection() {
    let (session, _turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let control = session.services.agent_control.clone();
    control.state.register_root_thread(session.thread_id);
    let parent = session.presentation_id();
    let child = session_presentation_id(ThreadId::new());
    let response_observation_transaction = control
        .acquire_response_observation_transaction(parent)
        .await;
    let automatic_session = Arc::clone(&session);
    let automatic_turn = tokio::spawn(async move {
        automatic_session
            .try_start_turn_if_idle(vec![TurnInput::ResponseItem(
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "continue active goal".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            )])
            .await
    });

    // The automatic reservation holds the mailbox while it waits for the observation transaction,
    // proving its initial idle checks have completed before this test binds the wake.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match tokio::time::timeout(
                Duration::from_millis(10),
                control.acquire_mailbox_submission_permit(parent.thread_id),
            )
            .await
            {
                Err(_) => break,
                Ok(Ok(permit)) => {
                    drop(permit);
                    tokio::task::yield_now().await;
                }
                Ok(Err(err)) => panic!("mailbox reservation failed: {err}"),
            }
        }
    })
    .await
    .expect("automatic turn should reach the observation transaction");

    let _watcher_registration = control
        .restore_response_watcher_with_admission(
            child,
            parent,
            &session.submission_admission,
            &codex_protocol::protocol::AgentResponseObservation {
                observer_thread_id: parent.thread_id,
                target_thread_id: child.thread_id,
                target_turn_id: Some("late-bound-turn".to_string()),
                task_preview: None,
                promoted_task_context: None,
                pending_commentary: false,
                commentary_after_sequences: Vec::new(),
                commentary_admissions: Vec::new(),
                commentary_delivery: None,
                target_messages: false,
                queue_delivery: false,
                message_wake_turn_id: None,
                baseline_final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
                final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::Wake,
                final_delivery_response_item_id: None,
                committed_delivery_response_item_ids: Vec::new(),
            },
        )
        .expect("late-bound watcher registration");
    drop(response_observation_transaction);

    let rejection = automatic_turn
        .await
        .expect("automatic reservation task")
        .expect_err("late-bound wake should win the idle reservation");
    assert_eq!(
        rejection.reason(),
        crate::codex_thread::TryStartTurnIfIdleRejectionReason::PendingTriggerTurn
    );
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn permanent_watcher_teardown_revokes_its_bound_wake() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let watcher_registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let watcher_guard =
        CompletionWatcherLifecycleGuard::new(control.clone(), watcher_registration, parent, child);

    assert!(control.has_bound_final_response_wake(parent));

    drop(watcher_guard);

    assert!(!control.has_bound_final_response_wake(parent));
}

#[test]
fn queued_turn_observation_retains_queued_source_delivery() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::Passive,
                /*target_messages*/ false,
                /*queue_input*/ true,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let response_item_id = codex_protocol::ResponseItemId::new("msg");

    assert_eq!(
        control.prepare_final_response_observation_delivery(
            parent,
            child,
            "turn-1",
            &response_item_id,
        ),
        (
            FinalResponseObservation::Passive,
            Some(response_item_id),
            true,
        )
    );
}

#[test]
fn fire_and_forget_audit_snapshot_records_canonical_target_without_a_watcher() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());

    let snapshots =
        control.response_observation_audit_snapshots(parent, child, Some("turn-1".to_string()));

    assert_eq!(
        snapshots,
        vec![codex_protocol::protocol::AgentResponseObservation {
            observer_thread_id: parent.thread_id,
            target_thread_id: child.thread_id,
            target_turn_id: Some("turn-1".to_string()),
            task_preview: None,
            promoted_task_context: None,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            target_messages: false,
            queue_delivery: false,
            message_wake_turn_id: None,
            baseline_final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        }]
    );
}

#[test]
fn observers_aggregate_the_same_target_turn_independently() {
    let control = AgentControl::default();
    let first_parent = session_presentation_id(ThreadId::new());
    let second_parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let first_admission = Arc::new(SubmissionAdmission::default());
    let second_admission = Arc::new(SubmissionAdmission::default());
    let _first_registration = control
        .register_response_watcher_with_admission(
            child,
            first_parent,
            &first_admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("first observer");
    let _second_registration = control
        .register_response_watcher_with_admission(
            child,
            second_parent,
            &second_admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::None,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("second observer");

    let first = control
        .response_observation_snapshots(first_parent, child)
        .into_iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("first observer turn");
    let second = control
        .response_observation_snapshots(second_parent, child)
        .into_iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("second observer turn");

    assert_eq!(
        (first.pending_commentary, first.final_delivery),
        (
            false,
            codex_protocol::protocol::AgentResponseFinalDelivery::Wake
        )
    );
    assert_eq!(
        (second.pending_commentary, second.final_delivery),
        (
            true,
            codex_protocol::protocol::AgentResponseFinalDelivery::None
        )
    );
}

#[test]
fn reattached_observer_generation_replaces_stale_parent_selection() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = session_presentation_id(parent_thread_id);
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child = session_presentation_id(ThreadId::new());
    let old_admission = Arc::new(SubmissionAdmission::default());
    let new_admission = Arc::new(SubmissionAdmission::default());
    let _old_registration = control
        .register_completion_watcher_with_admission(child, old_parent, &old_admission)
        .expect("old watcher registration");
    let _new_registration = control
        .register_completion_watcher_with_admission(child, new_parent, &new_admission)
        .expect("new watcher registration");

    assert_eq!(
        control.completion_parent_for_child(child, parent_thread_id),
        Some(new_parent)
    );
}

#[test]
fn finishing_a_target_turn_clears_only_that_turn_aggregate() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        ResponseObservationPolicy::from_parts(
            /*commentary*/ true,
            FinalResponseObservation::None,
        ),
        /*retain_passive_completion_relationship*/ false,
        Some("turn-2".to_string()),
        ResponseObservationBinding::NextTurn,
        ResponseObservationPersistence::Durable,
    );

    let updates = control.finish_response_observation_turn(parent, child, "turn-1");

    assert!(updates.iter().any(|snapshot| {
        snapshot.target_turn_id.as_deref() == Some("turn-1")
            && snapshot.final_delivery == codex_protocol::protocol::AgentResponseFinalDelivery::None
    }));
    assert!(updates.iter().any(|snapshot| {
        snapshot.target_turn_id.as_deref() == Some("turn-2") && snapshot.pending_commentary
    }));
}

#[test]
fn binding_a_pending_observation_persists_its_unbound_tombstone() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");

    control.bind_response_observation_turn(
        parent,
        child,
        "admitted-turn",
        ResponseObservationBinding::NextTurn,
    );

    let snapshots = control.response_observation_snapshots(parent, child);
    let pending = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.is_none())
        .expect("unbound tombstone");
    assert!(!pending.pending_commentary);
    assert_eq!(
        pending.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::None
    );
    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.target_turn_id.as_deref() == Some("admitted-turn"))
    );
}

#[test]
fn explicit_input_observation_waits_for_admission_turn_binding() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let admission_id = uuid::Uuid::now_v7();
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::ExplicitAdmission(admission_id),
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");

    assert!(!control.bind_response_observation_started_turn(parent, child, "unrelated-turn"));
    assert_eq!(
        control.response_observation_event_match(parent, child, "unrelated-turn"),
        ResponseObservationEventMatch::AwaitBinding
    );

    control.bind_response_observation_turn(
        parent,
        child,
        "admitted-turn",
        ResponseObservationBinding::ExplicitAdmission(admission_id),
    );

    assert_eq!(
        control.response_observation_event_match(parent, child, "unrelated-turn"),
        ResponseObservationEventMatch::Ignore
    );
    assert_eq!(
        control.response_observation_event_match(parent, child, "admitted-turn"),
        ResponseObservationEventMatch::Observe
    );
}

#[test]
fn concurrent_input_observations_bind_to_their_own_admitted_turns() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let commentary_admission = uuid::Uuid::now_v7();
    let final_admission = uuid::Uuid::now_v7();
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::None,
            ),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::ExplicitAdmission(commentary_admission),
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");
    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        ResponseObservationPolicy::from_parts(
            /*commentary*/ false,
            FinalResponseObservation::Wake,
        ),
        /*retain_passive_completion_relationship*/ false,
        /*target_turn_id*/ None,
        ResponseObservationBinding::ExplicitAdmission(final_admission),
        ResponseObservationPersistence::Durable,
    );

    control.bind_response_observation_turn(
        parent,
        child,
        "commentary-turn",
        ResponseObservationBinding::ExplicitAdmission(commentary_admission),
    );
    control.bind_response_observation_turn(
        parent,
        child,
        "final-turn",
        ResponseObservationBinding::ExplicitAdmission(final_admission),
    );

    let snapshots = control.response_observation_snapshots(parent, child);
    let commentary_turn = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("commentary-turn"))
        .expect("commentary turn");
    let final_turn = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("final-turn"))
        .expect("final turn");
    assert!(commentary_turn.pending_commentary);
    assert_eq!(
        commentary_turn.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::None
    );
    assert!(!final_turn.pending_commentary);
    assert_eq!(
        final_turn.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::Wake
    );
}

#[test]
fn failed_input_cancels_only_its_pending_observation() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let cancelled_admission = uuid::Uuid::now_v7();
    let retained_admission = uuid::Uuid::now_v7();
    let policy = ResponseObservationPolicy::from_parts(
        /*commentary*/ true,
        FinalResponseObservation::None,
    );
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            policy,
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::ExplicitAdmission(cancelled_admission),
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");
    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        policy,
        /*retain_passive_completion_relationship*/ false,
        /*target_turn_id*/ None,
        ResponseObservationBinding::ExplicitAdmission(retained_admission),
        ResponseObservationPersistence::Durable,
    );

    control.cancel_response_observation_admission(parent, child, cancelled_admission);
    control.bind_response_observation_turn(
        parent,
        child,
        "retained-turn",
        ResponseObservationBinding::ExplicitAdmission(retained_admission),
    );

    let snapshots = control.response_observation_snapshots(parent, child);
    assert_eq!(
        snapshots
            .iter()
            .filter_map(|snapshot| snapshot.target_turn_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["retained-turn"]
    );
}

#[test]
fn detached_durable_watcher_keeps_state_and_terminal_until_instance_migration() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let mut registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    registration.preserve_state_for_replacement_on_drop();
    drop(registration);

    assert!(
        control
            .response_observation_snapshots(parent, child)
            .iter()
            .any(|snapshot| {
                snapshot.final_delivery
                    == codex_protocol::protocol::AgentResponseFinalDelivery::Wake
            })
    );
    let terminal = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("replacement watcher terminal");
    assert_eq!(terminal.status, completed_status());
    control.clear_response_observation_relationship(parent, child);
    assert!(
        control
            .response_observation_snapshots(parent, child)
            .is_empty()
    );
}

#[test]
fn permanently_dropped_watcher_discards_queued_terminal_and_marker() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let registration = control
        .register_completion_watcher_with_admission(child, parent, &admission)
        .expect("watcher registration");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    assert_eq!(Arc::strong_count(&admission), 2);

    drop(registration);

    assert_eq!(Arc::strong_count(&admission), 1);
    assert!(
        control
            .take_watcher_terminal_presentation(parent, child)
            .is_none()
    );
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    assert!(
        control
            .take_watcher_terminal_presentation(parent, child)
            .is_some()
    );
}

#[test]
fn retiring_completed_turn_does_not_remove_newly_admitted_observation() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let mut registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("first watcher registration");
    control.finish_response_observation_turn(parent, child, "turn-1");

    assert!(
        control
            .register_response_watcher_with_admission(
                child,
                parent,
                &admission,
                ResponseObservationPolicy::from_parts(
                    /*commentary*/ false,
                    FinalResponseObservation::Wake,
                ),
                /*retain_passive_completion_relationship*/ false,
                Some("turn-2".to_string()),
                ResponseObservationBinding::NextTurn,
                ResponseObservationPersistence::Durable,
            )
            .is_none(),
        "the existing watcher should own the newly admitted turn"
    );

    assert!(!registration.retire_if_observation_idle());
    assert!(control.has_completion_watcher(parent, child));
    assert!(
        control
            .response_observation_snapshots(parent, child)
            .iter()
            .any(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-2"))
    );
}

#[test]
fn one_complete_commentary_satisfies_all_pending_commentary_requests() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let commentary = ResponseObservationPolicy::from_parts(
        /*commentary*/ true,
        FinalResponseObservation::None,
    );
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            commentary,
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("first registration");
    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        commentary,
        /*retain_passive_completion_relationship*/ false,
        Some("turn-1".to_string()),
        ResponseObservationBinding::NextTurn,
        ResponseObservationPersistence::Durable,
    );

    let first = control
        .prepare_commentary_observation_delivery(
            parent,
            child,
            "turn-1",
            "commentary-1",
            "Acknowledged.",
        )
        .expect("first commentary delivery");
    let second = control.prepare_commentary_observation_delivery(
        parent,
        child,
        "turn-1",
        "commentary-2",
        "Progress update.",
    );

    assert_eq!(first.source_item_id, "commentary-1");
    assert_eq!(first.text, "Acknowledged.");
    assert_eq!(second, None);

    control.commit_response_observation_delivery(&ResponseObservationDeliveryCommit {
        parent,
        child,
        turn_id: "turn-1".to_string(),
        response_item_id: first.response_item_id,
        kind: ResponseObservationDeliveryKind::Commentary,
    });
    let _ = control.register_response_watcher_with_admission(
        child,
        parent,
        &admission,
        commentary,
        /*retain_passive_completion_relationship*/ false,
        Some("turn-1".to_string()),
        ResponseObservationBinding::NextTurn,
        ResponseObservationPersistence::Durable,
    );
    assert!(
        control
            .prepare_commentary_observation_delivery(
                parent,
                child,
                "turn-1",
                "commentary-3",
                "New acknowledgement.",
            )
            .is_some()
    );
}

#[test]
fn commentary_observation_respects_each_admission_boundary() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let commentary = ResponseObservationPolicy::from_parts(
        /*commentary*/ true,
        FinalResponseObservation::None,
    );
    let _registration = control
        .register_response_watcher_with_admission_at_sequence(
            child,
            parent,
            &admission,
            commentary,
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 5,
            /*after_item_id*/ None,
        )
        .expect("watcher registration");
    let _ = control.register_response_watcher_with_admission_at_sequence(
        child,
        parent,
        &admission,
        commentary,
        /*retain_passive_completion_relationship*/ false,
        Some("turn-1".to_string()),
        ResponseObservationBinding::NextTurn,
        ResponseObservationPersistence::Durable,
        /*minimum_event_sequence*/ 10,
        /*after_item_id*/ None,
    );

    let first = control
        .prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            "turn-1",
            "commentary-1",
            "First acknowledgement.",
            /*sequence*/ 7,
        )
        .expect("first admitted commentary");
    control.commit_response_observation_delivery(&ResponseObservationDeliveryCommit {
        parent,
        child,
        turn_id: "turn-1".to_string(),
        response_item_id: first.response_item_id,
        kind: ResponseObservationDeliveryKind::Commentary,
    });
    assert!(
        control
            .prepare_commentary_observation_delivery_at_sequence(
                parent,
                child,
                "turn-1",
                "commentary-2",
                "Too early.",
                /*sequence*/ 9,
            )
            .is_none()
    );
    let second = control
        .prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            "turn-1",
            "commentary-3",
            "Second acknowledgement.",
            /*sequence*/ 10,
        )
        .expect("second admitted commentary");

    assert_eq!(
        (first.source_item_id, second.source_item_id),
        ("commentary-1".to_string(), "commentary-3".to_string())
    );
}

#[test]
fn recovered_commentary_uses_the_canonical_source_item_boundary() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let commentary = ResponseObservationPolicy::from_parts(
        /*commentary*/ true,
        FinalResponseObservation::None,
    );
    let _registration = control
        .register_response_watcher_with_admission_at_sequence(
            child,
            parent,
            &admission,
            commentary,
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 2,
            Some("commentary-before-admission".to_string()),
        )
        .expect("watcher registration");
    let mut prior_item_ids = HashSet::new();

    assert!(
        control
            .prepare_recovered_commentary_observation_delivery(
                parent,
                child,
                "turn-1",
                "stale-commentary",
                "Written before admission.",
                /*sequence*/ 42,
                &prior_item_ids,
            )
            .is_none()
    );
    prior_item_ids.insert("commentary-before-admission".to_string());
    let delivery = control
        .prepare_recovered_commentary_observation_delivery(
            parent,
            child,
            "turn-1",
            "commentary-after-admission",
            "Written after admission.",
            /*sequence*/ 43,
            &prior_item_ids,
        )
        .expect("first commentary after canonical admission boundary");

    assert_eq!(
        delivery.source_item_id,
        "commentary-after-admission".to_string()
    );
}

#[test]
fn durable_delivery_suffix_commits_without_mutating_live_state_first() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ true,
                FinalResponseObservation::None,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let delivery = control
        .prepare_commentary_observation_delivery(
            parent,
            child,
            "turn-1",
            "commentary-1",
            "Acknowledged.",
        )
        .expect("commentary delivery");

    let committed = control.response_observation_committed_snapshots(
        parent,
        child,
        "turn-1",
        &delivery.response_item_id,
        ResponseObservationDeliveryKind::Commentary,
    );

    let committed_turn = committed
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("committed turn");
    assert_eq!(committed_turn.commentary_delivery, None);
    assert_eq!(
        committed_turn.committed_delivery_response_item_ids,
        vec![delivery.response_item_id.clone()]
    );
    assert_eq!(
        control
            .response_observation_commentary_delivery(parent, child, "turn-1")
            .map(|delivery| delivery.response_item_id),
        Some(delivery.response_item_id)
    );
}

#[test]
fn reconstructed_terminal_reuses_the_persisted_final_delivery_identity() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let first = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("first terminal presentation");
    let first_response_item_id = first.presentation.completion_context_response_item_id();
    let _ = control.prepare_final_response_observation_delivery(
        parent,
        child,
        "turn-1",
        &first_response_item_id,
    );
    control.finish_watcher_terminal_presentation(parent, child, "turn-1");

    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let restored = control
        .take_watcher_terminal_presentation(parent, child)
        .expect("restored terminal presentation");

    assert_eq!(
        restored.presentation.completion_context_response_item_id(),
        first_response_item_id
    );
}

#[test]
fn wait_claim_commits_the_effective_final_observation_for_its_target_turn() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);
    let _ = control.record_agent_terminal_presentation(
        parent,
        child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let commit = wait.freeze_for_children([child.thread_id]);
    let claimed_target_turns = commit.claimed_target_turns();

    let snapshots =
        control.wait_response_observation_committed_snapshots(parent, &claimed_target_turns);

    let turn = snapshots
        .iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("claimed turn snapshot");
    let response_item_id = turn
        .final_delivery_response_item_id
        .as_ref()
        .expect("stable final delivery id");
    assert_eq!(
        (
            response_item_id.clone(),
            turn.committed_delivery_response_item_ids.clone(),
        ),
        (
            claimed_target_turns[0].response_item_id.clone(),
            vec![response_item_id.clone()],
        )
    );
}

#[test]
fn cancelled_or_timed_out_wait_does_not_consume_a_final_wake() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            child,
            parent,
            &admission,
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some("turn-1".to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("watcher registration");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child.thread_id]);

    drop(wait);

    let turn = control
        .response_observation_snapshots(parent, child)
        .into_iter()
        .find(|snapshot| snapshot.target_turn_id.as_deref() == Some("turn-1"))
        .expect("observed turn");
    assert_eq!(
        turn.final_delivery,
        codex_protocol::protocol::AgentResponseFinalDelivery::Wake
    );
}
