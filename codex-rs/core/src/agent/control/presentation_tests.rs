use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::protocol::AgentResponseCommentaryAdmission;
use pretty_assertions::assert_eq;
use response_observation::ResponseTurnObservation;

fn identity() -> SessionPresentationId {
    SessionPresentationId::new(ThreadId::new(), Uuid::now_v7())
}

#[test]
fn next_turn_policy_does_not_block_a_delayed_historical_terminal() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                pending_next_turn: Some(ResponseTurnObservation {
                    final_response: FinalResponseObservation::Wake,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
    assert_eq!(
        control.response_observation_event_match(parent, child, "old-turn"),
        ResponseObservationEventMatch::Ignore,
    );
    assert!(control.bind_response_observation_started_turn_at_sequence(
        parent, child, "new-turn", /*sequence*/ 10,
    ));
    assert_eq!(
        control.response_observation_event_match(parent, child, "new-turn"),
        ResponseObservationEventMatch::Observe,
    );
}

#[test]
fn target_message_admission_reserves_one_idle_wake() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                turns: HashMap::from([(
                    "child-turn".to_owned(),
                    ResponseTurnObservation {
                        target_messages: true,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );

    let TargetMessageAdmission::Wake(reservation_id) = control
        .target_message_admission(
            parent,
            child,
            "child-turn",
            None,
            None,
            TargetMessageAdmissionMode::SteerOrWake,
        )
        .expect("message route should reserve an idle wake")
    else {
        panic!("expected idle wake reservation");
    };
    assert_eq!(
        control
            .target_message_admission(
                parent,
                child,
                "child-turn",
                None,
                None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("pending wake should remain visible"),
        TargetMessageAdmission::PendingWake
    );
    assert!(control.commit_target_message_wake(
        parent,
        child,
        "child-turn",
        reservation_id,
        "source-turn",
    ));
    assert_eq!(
        control
            .target_message_admission(
                parent,
                child,
                "child-turn",
                Some("source-turn"),
                None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .expect("active source turn should be steerable"),
        TargetMessageAdmission::Steer
    );

    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .get_mut(&(parent, child))
        .expect("the exact relationship should still exist")
        .revoked = true;
    assert!(
        control
            .target_message_admission(
                parent,
                child,
                "child-turn",
                Some("source-turn"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err(),
        "retained turn data must not authorize input through a revoked relationship"
    );
}

#[test]
fn future_policy_replacement_retains_pending_message_and_queue_axes() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    let replacement = SessionPresentationId::new(child.thread_id, Uuid::now_v7());
    let expected_policy = ResponseObservationPolicy::from_turn_parts(
        /*commentary*/ false,
        FinalResponseObservation::Wake,
        /*target_messages*/ true,
        /*queue_input*/ true,
    );
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                pending_next_turn: Some(ResponseTurnObservation {
                    final_response: FinalResponseObservation::Wake,
                    target_messages: true,
                    queue_delivery: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
    assert_eq!(
        control.reserved_response_observation_policy(parent, child),
        Some(expected_policy)
    );
    assert!(control.move_future_response_observation(parent, child, replacement));
    assert_eq!(
        (
            control.reserved_response_observation_policy(parent, child),
            control.reserved_response_observation_policy(parent, replacement),
        ),
        (None, Some(expected_policy))
    );
    assert!(
        !control.target_message_binding_pending(parent, replacement),
        "a future-turn policy must not block a current sender waiting for new authority"
    );
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .get_mut(&(parent, replacement))
        .expect("the replacement relationship should exist")
        .pending_admissions
        .insert(
            Uuid::now_v7(),
            ResponseTurnObservation {
                target_messages: true,
                ..Default::default()
            },
        );
    assert!(control.target_message_binding_pending(parent, replacement));
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .get_mut(&(parent, replacement))
        .expect("the replacement relationship should exist")
        .revoked = true;
    assert!(!control.target_message_binding_pending(parent, replacement));
    assert_eq!(
        control.reserved_response_observation_policy(parent, replacement),
        None
    );
}

#[test]
fn admitted_policy_binds_actual_turn_and_cannot_downgrade_an_accepted_final() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    let admission = Uuid::now_v7();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                persistence: ResponseObservationPersistence::Durable,
                pending_admissions: HashMap::from([(
                    admission,
                    ResponseTurnObservation {
                        final_response: FinalResponseObservation::Wake,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    assert_eq!(
        control.response_observation_event_match(parent, child, "actual-turn"),
        ResponseObservationEventMatch::AwaitBinding,
    );
    control.bind_response_observation_turn_at_sequence(
        parent,
        child,
        "actual-turn",
        ResponseObservationBinding::ExplicitAdmission(admission),
        /*commentary_boundary*/ None,
        ResponseObservationBindingPublication::Immediate,
    );
    let context = new_sub_agent_completion_context_response_item_id();
    assert_eq!(
        control.prepare_final_response_observation_delivery(parent, child, "actual-turn", &context),
        (FinalResponseObservation::Wake, Some(context.clone())),
    );
    let later_admission = Uuid::now_v7();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .get_mut(&(parent, child))
        .unwrap()
        .pending_admissions
        .insert(
            later_admission,
            ResponseTurnObservation {
                final_response: FinalResponseObservation::PresentationOnly,
                ..Default::default()
            },
        );
    let before = control.response_observation_snapshots(parent, child);
    control.bind_response_observation_turn_at_sequence(
        parent,
        child,
        "actual-turn",
        ResponseObservationBinding::ExplicitAdmission(later_admission),
        /*commentary_boundary*/ None,
        ResponseObservationBindingPublication::Immediate,
    );
    assert_eq!(
        control.response_observation_snapshots(parent, child),
        before
    );
    assert_eq!(
        control.response_observation_event_match(parent, child, &admission.to_string()),
        ResponseObservationEventMatch::Ignore,
    );
}

#[test]
fn commentary_boundary_delivers_once_and_retains_committed_evidence() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                persistence: ResponseObservationPersistence::Durable,
                turns: HashMap::from([(
                    "turn".to_owned(),
                    ResponseTurnObservation {
                        commentary_admissions: vec![AgentResponseCommentaryAdmission {
                            minimum_event_sequence: 12,
                            after_item_id: Some("before".to_owned()),
                            canonical_boundary: true,
                        }],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    assert_eq!(
        control.prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            "turn",
            "early",
            "too early",
            /*sequence*/ 11,
        ),
        None,
    );
    let text = "complete commentary 🦀 ".repeat(600);
    let delivery = control
        .prepare_commentary_observation_delivery_at_sequence(
            parent, child, "turn", "eligible", &text, /*sequence*/ 12,
        )
        .unwrap();
    assert_eq!(delivery.text, text);
    let commit = ResponseObservationDeliveryCommit {
        parent,
        child,
        turn_id: "turn".to_owned(),
        response_item_id: delivery.response_item_id.clone(),
        kind: ResponseObservationDeliveryKind::Commentary,
        model_visibility: codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
    };
    let committed = control.deferred_response_observation_commit_snapshots(&commit);
    control.commit_response_observation_delivery(&commit);
    assert_eq!(
        control.response_observation_snapshots(parent, child),
        committed
    );
    assert_eq!(
        control.prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            "turn",
            "later",
            "not requested",
            /*sequence*/ 13,
        ),
        None,
    );
    assert_eq!(
        control.finish_response_observation_turn(parent, child, "turn"),
        committed
    );
}

#[test]
fn every_final_disposition_commits_one_identity_including_presentation_only() {
    for disposition in [
        FinalResponseObservation::PresentationOnly,
        FinalResponseObservation::Passive,
        FinalResponseObservation::Wake,
    ] {
        let control = LocalAgentControl::default();
        let parent = identity();
        let child = identity();
        control
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .insert(
                (parent, child),
                ResponseObserverRelationship {
                    persistence: ResponseObservationPersistence::Durable,
                    turns: HashMap::from([(
                        "turn".to_owned(),
                        ResponseTurnObservation {
                            final_response: disposition,
                            ..Default::default()
                        },
                    )]),
                    ..Default::default()
                },
            );
        let context = new_sub_agent_completion_context_response_item_id();
        assert_eq!(
            control.prepare_final_response_observation_delivery(parent, child, "turn", &context),
            (disposition, Some(context.clone())),
        );
        let commit = ResponseObservationDeliveryCommit {
            parent,
            child,
            turn_id: "turn".to_owned(),
            response_item_id: context.clone(),
            kind: ResponseObservationDeliveryKind::Final,
            model_visibility: if disposition == FinalResponseObservation::PresentationOnly {
                codex_protocol::protocol::SubAgentCompletionModelVisibility::NotVisible
            } else {
                codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible
            },
        };
        let snapshots = control.deferred_response_observation_commit_snapshots(&commit);
        control.commit_response_observation_delivery(&commit);
        assert_eq!(
            control.response_observation_snapshots(parent, child),
            snapshots
        );
        assert_eq!(
            control.prepare_final_response_observation_delivery(parent, child, "turn", &context),
            (FinalResponseObservation::None, None),
        );
    }
}

#[test]
fn residency_transfer_moves_only_future_policy_and_keeps_old_delivery_identity() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let old_child = identity();
    let new_child = SessionPresentationId::new(old_child.thread_id, Uuid::now_v7());
    let context = new_sub_agent_completion_context_response_item_id();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, old_child),
            ResponseObserverRelationship {
                persistence: ResponseObservationPersistence::Durable,
                baseline_final_response: FinalResponseObservation::Passive,
                pending_next_turn: Some(ResponseTurnObservation {
                    final_response: FinalResponseObservation::Wake,
                    commentary_admissions: vec![AgentResponseCommentaryAdmission {
                        minimum_event_sequence: 900,
                        after_item_id: Some("old-runtime-item".to_owned()),
                        canonical_boundary: true,
                    }],
                    ..Default::default()
                }),
                turns: HashMap::from([(
                    "old-turn".to_owned(),
                    ResponseTurnObservation {
                        final_response: FinalResponseObservation::Wake,
                        final_delivery_response_item_id: Some(context.clone()),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    let mut expected_old = control.response_observation_snapshots(parent, old_child);
    for snapshot in &mut expected_old {
        snapshot.baseline_final_delivery =
            codex_protocol::protocol::AgentResponseFinalDelivery::None;
        if snapshot.target_turn_id.is_none() {
            snapshot.final_delivery = codex_protocol::protocol::AgentResponseFinalDelivery::None;
            snapshot.pending_commentary = false;
            snapshot.commentary_admissions.clear();
        }
    }
    assert!(control.move_future_response_observation(parent, old_child, new_child));
    assert_eq!(
        control.response_observation_snapshots(parent, old_child),
        expected_old
    );
    assert!(control.has_future_response_observation(parent, new_child));
    assert_eq!(
        control
            .prepare_final_response_observation_delivery(parent, new_child, "old-turn", &context),
        (FinalResponseObservation::None, None),
    );
    assert!(control.bind_response_observation_started_turn_at_sequence(
        parent, new_child, "new-turn", /*sequence*/ 0,
    ));
    assert_eq!(
        control
            .prepare_final_response_observation_delivery(parent, new_child, "new-turn", &context),
        (FinalResponseObservation::Wake, Some(context)),
    );
    let commentary = control
        .prepare_commentary_observation_delivery_at_sequence(
            parent,
            new_child,
            "new-turn",
            "new-item",
            "new runtime commentary",
            /*sequence*/ 1,
        )
        .unwrap();
    assert_eq!(commentary.text, "new runtime commentary");
}

#[test]
fn close_after_claim_yields_inert_exact_turn_committed_tombstone() {
    let control = LocalAgentControl::default();
    let commit = ResponseObservationDeliveryCommit {
        parent: identity(),
        child: identity(),
        turn_id: "accepted-turn".to_owned(),
        response_item_id: new_sub_agent_completion_context_response_item_id(),
        kind: ResponseObservationDeliveryKind::Final,
        model_visibility: codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
    };
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (commit.parent, commit.child),
            ResponseObserverRelationship {
                persistence: ResponseObservationPersistence::Durable,
                baseline_final_response: FinalResponseObservation::Passive,
                turns: HashMap::from([(
                    commit.turn_id.clone(),
                    ResponseTurnObservation {
                        final_response: FinalResponseObservation::Wake,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    assert_eq!(
        control.prepare_final_response_observation_delivery(
            commit.parent,
            commit.child,
            &commit.turn_id,
            &commit.response_item_id,
        ),
        (
            FinalResponseObservation::Wake,
            Some(commit.response_item_id.clone())
        ),
    );
    control.revoke_response_observations_for_child(commit.child);
    let tombstone = control.deferred_response_observation_commit_snapshots(&commit);
    assert_eq!(
        tombstone.last(),
        Some(&codex_protocol::protocol::AgentResponseObservation {
            observer_thread_id: commit.parent.thread_id,
            target_thread_id: commit.child.thread_id,
            target_turn_id: Some(commit.turn_id.clone()),
            task_preview: None,
            promoted_task_context: None,
            target_messages: false,
            reply_route_enabled: None,
            reply_route_context_installed: false,
            queue_delivery: false,
            message_wake_turn_id: None,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery_response_item_id: Some(commit.response_item_id.clone()),
            committed_delivery_response_item_ids: vec![commit.response_item_id.clone()],
        })
    );
    control.commit_response_observation_delivery(&commit);
    assert_eq!(
        control.response_observation_snapshots(commit.parent, commit.child),
        tombstone
    );
}

fn terminal(
    state: &WaitAgentPresentations,
    parent: SessionPresentationId,
    child: SessionPresentationId,
) -> AgentTerminalPresentation {
    let mut state = state.state.lock().unwrap();
    let waits = state
        .waits
        .iter()
        .filter_map(|(id, wait)| {
            (wait.parent == parent
                && wait
                    .children
                    .as_ref()
                    .is_none_or(|children| children.contains(&child.thread_id)))
            .then_some(*id)
        })
        .collect::<HashSet<_>>();
    let sequence = state.next_terminal;
    state.next_terminal += 1;
    let inner = Arc::new(Terminal {
        parent,
        child,
        turn_id: format!("turn-{sequence}"),
        sequence,
        parent_thread: Mutex::new(None),
        context_id: new_sub_agent_completion_context_response_item_id(),
        status: AgentStatus::Completed(Some("captured result".to_owned())),
        presentation: CompletionPresentation {
            item: TurnItem::AgentMessage(
                codex_protocol::protocol::sub_agent_completion_item(
                    "/root/child",
                    &AgentStatus::Completed(Some("captured result".to_owned())),
                )
                .expect("terminal presentation"),
            ),
            history_only_turn_id: Uuid::now_v7().to_string(),
        },
        observation_presentation: OnceLock::new(),
        accepted: Mutex::new(None),
        ownership: Mutex::new(Ownership {
            waits: waits.clone(),
            presenter: None,
            background_claimed: false,
            committed: false,
        }),
        changed: Notify::new(),
    });
    for id in waits {
        state
            .waits
            .get_mut(&id)
            .unwrap()
            .terminals
            .push(Arc::downgrade(&inner));
    }
    state
        .contexts
        .insert(inner.context_id.clone(), Arc::clone(&inner));
    state
        .response_terminals
        .insert((parent, child, inner.turn_id.clone()), Arc::clone(&inner));
    AgentTerminalPresentation { inner }
}

#[tokio::test]
async fn late_wait_claims_only_latest_turn_and_releases_old_turn() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let old = terminal(&state, parent, child);
    let current = terminal(&state, parent, child);
    let wait = state.register(parent, Some(HashSet::from([child.thread_id])));
    let commit = wait.freeze_for_terminal_statuses(&HashMap::from([(
        child.thread_id,
        (
            Some(current.inner.turn_id.clone()),
            current.inner.status.clone(),
        ),
    )]));
    assert_eq!(
        commit.claimed_target_turns(),
        vec![ClaimedTargetTurn {
            child,
            turn_id: current.inner.turn_id.clone(),
            response_item_id: current.completion_context_response_item_id(),
        }]
    );
    assert!(!old.wait_owns_presentation().await);
    commit.commit();
    assert!(current.wait_owns_presentation().await);
}

#[test]
fn hidden_observation_freezes_a_trusted_row_without_replacing_the_native_receipt() {
    let state = WaitAgentPresentations::default();
    let presentation = terminal(&state, identity(), identity());
    let context_id = presentation.completion_context_response_item_id();
    let native = presentation.completion_presentation().item.clone();
    let hidden = presentation
        .hidden_observation_presentation("/root/child")
        .expect("hidden completion row");
    let TurnItem::AgentMessage(message) = &hidden.item else {
        panic!("completion must be an agent message");
    };
    assert!(message.has_sub_agent_completion_identity());
    assert_eq!(
        codex_protocol::protocol::sub_agent_completion_model_visibility_from_response_item_id(
            &message.id
        ),
        Some(codex_protocol::protocol::SubAgentCompletionModelVisibility::NotVisible),
    );
    assert_eq!(
        &presentation
            .hidden_observation_presentation("/different/later/reference")
            .expect("already frozen row")
            .item,
        &hidden.item,
    );
    assert_eq!(presentation.completion_presentation().item, native);
    assert_eq!(
        presentation.completion_context_response_item_id(),
        context_id
    );
    assert_eq!(
        hidden.history_only_turn_id,
        presentation.completion_presentation().history_only_turn_id,
    );
}

#[test]
fn close_between_terminal_reservation_and_response_claim_preserves_policy_and_identity() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    let terminal = terminal(&control.wait_agent_presentations, parent, child);
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                persistence: ResponseObservationPersistence::Durable,
                turns: HashMap::from([(
                    terminal.inner.turn_id.clone(),
                    ResponseTurnObservation {
                        final_response: FinalResponseObservation::Wake,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    control.revoke_response_observations_for_child(child);
    let repeated = control
        .record_response_observation_terminal(
            parent,
            child,
            &terminal.inner.turn_id,
            terminal.inner.status.clone(),
        )
        .unwrap();
    assert!(Arc::ptr_eq(&repeated.inner, &terminal.inner));
    assert_eq!(
        control.prepare_final_response_observation_delivery(
            parent,
            child,
            &terminal.inner.turn_id,
            &terminal.inner.context_id,
        ),
        (
            FinalResponseObservation::Wake,
            Some(terminal.inner.context_id.clone())
        ),
    );
    assert!(!control.has_future_response_observation(parent, child));
}

#[tokio::test]
async fn committed_wait_suppresses_background_presentation() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let wait = state.register(parent, Some(HashSet::from([child.thread_id])));
    let terminal = terminal(&state, parent, child);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            child.thread_id,
            AgentStatus::Completed(Some("captured result".to_owned()))
        ),])
    );
    commit.commit();
    assert!(terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn dropped_frozen_wait_releases_background_delivery() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let wait = state.register(parent, None);
    let terminal = terminal(&state, parent, child);
    drop(wait.freeze_for_children([child.thread_id]));
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn wait_claims_late_commentary_and_notifies_background_delivery_on_drop() {
    let control = LocalAgentControl::default();
    let parent = identity();
    let child = identity();
    let wait = control
        .wait_agent_presentations
        .register(parent, Some(HashSet::from([child.thread_id])));
    let terminal = terminal(&control.wait_agent_presentations, parent, child);
    let turn_id = terminal.inner.turn_id.clone();
    control
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .insert(
            (parent, child),
            ResponseObserverRelationship {
                turns: HashMap::from([(
                    turn_id.clone(),
                    ResponseTurnObservation {
                        commentary_admissions: vec![AgentResponseCommentaryAdmission {
                            minimum_event_sequence: 12,
                            after_item_id: None,
                            canonical_boundary: true,
                        }],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
    let mut commit = wait.freeze_for_children([child.thread_id]);
    let target_turns = commit.claimed_target_turns();
    commit.claim_commentary_turns(&target_turns);
    assert_eq!(
        control.route_response_observer_commentary(parent, child, &turn_id),
        CommentaryDeliveryRoute::Wait,
    );
    let pending = control.wait_commentary_before_terminal(parent, &target_turns);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());

    let delivery = control
        .prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            &turn_id,
            "late-commentary",
            "commentary before the terminal",
            /*sequence*/ 12,
        )
        .expect("eligible commentary");
    control.publish_response_observation_binding();
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("wait should observe late commentary");
    assert_eq!(
        delivered
            .into_iter()
            .map(|delivery| (delivery.child, delivery.turn_id, delivery.delivery))
            .collect::<Vec<_>>(),
        vec![(child, turn_id.clone(), delivery)],
    );
    assert_eq!(
        control.route_response_observer_commentary(parent, child, &turn_id),
        CommentaryDeliveryRoute::Wait,
    );

    let changed = control.response_observation_changed().notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    drop(commit);
    tokio::time::timeout(std::time::Duration::from_secs(5), changed)
        .await
        .expect("dropping a wait should wake the retained commentary publisher");
    assert_eq!(
        control.route_response_observer_commentary(parent, child, &turn_id),
        CommentaryDeliveryRoute::Mailbox,
    );
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn concurrent_waits_cannot_both_claim_the_same_terminal() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let first = state.register(parent, None);
    let second = state.register(parent, None);
    let terminal = terminal(&state, parent, child);
    let first = first.freeze_for_children([child.thread_id]);
    let second = second.freeze_for_children([child.thread_id]);
    assert_eq!(second.completion_presentation_agent_ids(), None);
    second.commit();
    drop(first);
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_mailbox_wait_cannot_claim_background_already_selected() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let terminal = terminal(&state, parent, identity());
    assert!(!terminal.wait_owns_presentation().await);
    let wait = state.register(parent, None);
    let commit = wait
        .freeze_for_mailbox_response_item_ids(&[terminal.completion_context_response_item_id()]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            terminal.inner.child.thread_id,
            AgentStatus::Completed(Some("captured result".to_owned())),
        )])
    );
    assert_eq!(commit.completion_presentation_agent_ids(), None);
    commit.commit();
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn replacement_runtime_cannot_claim_prior_mailbox_terminal() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let terminal = terminal(&state, parent, identity());
    let replacement = SessionPresentationId::new(parent.thread_id, Uuid::now_v7());
    let wait = state.register(replacement, None);
    let commit = wait
        .freeze_for_mailbox_response_item_ids(&[terminal.completion_context_response_item_id()]);
    assert_eq!(commit.agent_states(), HashMap::new());
    commit.commit();
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn freeze_releases_unselected_children() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let selected = identity();
    let wait = state.register(parent, None);
    let selected_terminal = terminal(&state, parent, selected);
    let unselected_terminal = terminal(&state, parent, identity());
    let commit = wait.freeze_for_children([selected.thread_id]);
    assert!(!unselected_terminal.wait_owns_presentation().await);
    commit.commit();
    assert!(selected_terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn mailbox_wait_displays_all_matches_but_owns_only_unclaimed_terminals() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let old = terminal(&state, parent, identity());
    assert!(!old.wait_owns_presentation().await);
    let wait = state.register(parent, None);
    let current = terminal(&state, parent, identity());
    let commit = wait.freeze_for_mailbox_response_item_ids(&[
        old.completion_context_response_item_id(),
        current.completion_context_response_item_id(),
    ]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([
            (old.inner.child.thread_id, old.inner.status.clone()),
            (current.inner.child.thread_id, current.inner.status.clone()),
        ])
    );
    assert_eq!(
        commit.completion_presentation_agent_ids(),
        Some(vec![current.inner.child.thread_id])
    );
    commit.commit();
    assert!(!old.wait_owns_presentation().await);
    assert!(current.wait_owns_presentation().await);
}
