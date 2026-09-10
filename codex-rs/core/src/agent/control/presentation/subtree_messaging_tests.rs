use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_thread_store::InMemoryThreadStoreFailure;
use futures::poll;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[tokio::test]
async fn failed_policy_context_does_not_install_turn_cache_or_sampling_input() {
    for failure in [
        InMemoryThreadStoreFailure::SubAgentCompletionAppend,
        InMemoryThreadStoreFailure::SubAgentCompletionPrefix,
        InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush,
    ] {
        let (mut session, _, _events) = make_session_and_context_with_rx().await;
        let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
        let control = session.services.agent_control.clone();
        assert!(session.begin_agent_response_turn("active-turn"));
        let key = "policy-cache".to_owned();
        let item = ContextualUserFragment::into(messaging_context::PermissionNotice {
            key: key.clone(),
            text: "User disabled sending".to_owned(),
        });
        store.fail_next_operation(failure).await;
        assert!(
            control
                .record_messaging_context(&session, key, item)
                .await
                .is_err(),
        );
        assert!(
            control
                .wait_agent_presentations
                .state()
                .pending_messaging_context
                .is_empty()
        );
        assert!(session.clone_history().await.annotated_items().is_empty());
        assert!(
            !session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await
        );
        assert!(session.submission_admission.requires_reload());
        assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
    }
}

#[tokio::test]
async fn policy_cache_ack_survives_cancelled_waiter_without_sampling_input() {
    let (mut session, _, _events) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(Arc::get_mut(&mut session).unwrap()).await;
    let control = session.services.agent_control.clone();
    assert!(session.begin_agent_response_turn("active-turn"));
    let key = "policy-cache".to_owned();
    let item = ContextualUserFragment::into(messaging_context::PermissionNotice {
        key: key.clone(),
        text: "User enabled sending".to_owned(),
    });
    let permit = session.reserve_history_publication().await;
    let mut pending = Box::pin(control.record_messaging_context(&session, key.clone(), item));
    assert!(poll!(&mut pending).is_pending());
    assert!(
        control
            .wait_agent_presentations
            .state()
            .pending_messaging_context
            .is_empty()
    );
    drop(pending);
    drop(permit);
    let cached = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let cached = control
                .wait_agent_presentations
                .state()
                .pending_messaging_context
                .get(&(session.presentation_id(), key.clone()))
                .cloned();
            if let Some(cached) = cached {
                break cached;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("owned publication installs cache after ACK");
    let recorded = session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        cached,
        (Some("active-turn".to_owned()), recorded[0].clone())
    );
    assert_eq!(recorded.len(), 1);
    assert!(
        !session
            .input_queue
            .has_pending_input(&session.active_turn)
            .await
    );
    assert!(!session.submission_admission.requires_reload());
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
}

#[tokio::test]
async fn standalone_turn_refresh_does_not_require_a_thread_manager() {
    let control = LocalAgentControl::default();
    let current = ThreadId::new();
    control
        .refresh_subtree_messaging(current)
        .await
        .expect("standalone turns have no durable agent policy to restore");
    control
        .refresh_subtree_messaging(current)
        .await
        .expect("repeated standalone turns remain independent of a manager");
    let state = control.wait_agent_presentations.state();
    assert!(state.subtree_messaging.is_empty());
    assert!(state.inherited_message_routes.is_empty());
    assert!(state.pending_messaging_context.is_empty());
}

#[tokio::test]
async fn missing_manager_still_fails_when_live_messaging_authority_needs_reconciliation() {
    let control = LocalAgentControl::default();
    let current = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    control
        .wait_agent_presentations
        .state()
        .subtree_messaging
        .insert(current, (0, TargetMessageRouteMode::Enabled));
    let error = control
        .refresh_subtree_messaging(current.thread_id)
        .await
        .expect_err("live authority must not silently bypass reconciliation");
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::UnsupportedOperation(_)
    ));
    assert_eq!(
        control.wait_agent_presentations.state().subtree_messaging,
        HashMap::from([(current, (0, TargetMessageRouteMode::Enabled))]),
    );
}

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
    let control = LocalAgentControl::default();
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
    assert!(control.target_message_wake_is_current(
        &crate::agent::turn_queue::QueuedTargetMessageWake {
            observer: recipient,
            target: sender,
            target_turn_id: "sender-turn".into(),
            reservation_id: reservation
        }
    ));
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Disabled);
    assert!(!control.target_message_wake_is_current(
        &crate::agent::turn_queue::QueuedTargetMessageWake {
            observer: recipient,
            target: sender,
            target_turn_id: "sender-turn".into(),
            reservation_id: reservation
        }
    ));
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
    let control = LocalAgentControl::default();
    let recipient = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let sender = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    control
        .wait_agent_presentations
        .state()
        .inherited_message_routes
        .insert((recipient, sender), TargetMessageRouteMode::Enabled);
    let prepared = control.prepare_reply_route(recipient, sender, TargetMessageRouteMode::Disabled);
    control
        .commit_reply_route(prepared.expect("prepared route"))
        .expect("commit route");
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
    let prepared = control.prepare_reply_route(recipient, sender, TargetMessageRouteMode::Enabled);
    control
        .commit_reply_route(prepared.expect("prepared route"))
        .expect("commit route");
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
