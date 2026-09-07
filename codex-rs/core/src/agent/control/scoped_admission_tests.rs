use super::*;
use crate::UserAgentReplyRouteMode;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
use test_case::test_case;

#[derive(Clone, Copy)]
enum Delivery {
    Wake,
    Steer,
}

#[derive(Clone, Copy)]
enum Revocation {
    Directed,
    Subtree,
}

struct FinishableTask {
    started: Arc<tokio::sync::Notify>,
    finish: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl SessionTask for FinishableTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "scoped_admission_test.finishable"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        session
            .send_event(
                ctx.as_ref(),
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: ctx.sub_id.clone(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                }),
            )
            .await;
        self.started.notify_one();
        let finish = self.finish.lock().await.take().expect("finish signal");
        tokio::select! {
            _ = finish => {}
            () = cancellation_token.cancelled() => {}
        }
        Ok(None)
    }
}

#[tokio::test]
async fn validated_scoped_steer_rejects_a_different_receiver_turn_at_submission() {
    const SENDER_TURN: &str = "scoped-sender";
    const T1: &str = "consumed-wake-t1";
    const T2: &str = "unrelated-user-t2";
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = root.session.services.agent_control.clone();
    let sender = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn sender");
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn receiver");
    let sender = harness
        .manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender");
    let receiver = harness
        .manager
        .get_thread(receiver.target_thread_id)
        .await
        .expect("receiver");
    let source = sender.session.presentation_id();
    let target = receiver.session.presentation_id();
    let admission = Arc::new(crate::session::SubmissionAdmission::default());
    let _registration = control
        .register_response_watcher_with_admission(
            source,
            target,
            &admission,
            ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
                /*target_messages*/ true,
                /*queue_input*/ false,
            ),
            /*retain_passive_completion_relationship*/ false,
            Some(SENDER_TURN.to_string()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("turn-scoped route");
    let TargetMessageAdmission::Wake(reservation_id) = control
        .target_message_admission(
            target,
            source,
            SENDER_TURN,
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SteerOrWake,
        )
        .expect("reserve the sole wake")
    else {
        panic!("expected idle wake");
    };
    assert!(control.commit_target_message_wake(target, source, SENDER_TURN, reservation_id, T1));
    let sender_started = Arc::new(tokio::sync::Notify::new());
    let sender_turn = sender
        .session
        .new_turn_with_default_settings(SENDER_TURN.to_string(), Default::default())
        .await;
    sender
        .session
        .spawn_task(
            sender_turn,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Regular,
                turn_start_gate: None,
                turn_start_attempted: Some(Arc::clone(&sender_started)),
            },
        )
        .await;
    timeout(Duration::from_secs(10), sender_started.notified())
        .await
        .expect("sender started");
    let t1_started = Arc::new(tokio::sync::Notify::new());
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let t1 = receiver
        .session
        .new_turn_with_default_settings(T1.to_string(), Default::default())
        .await;
    receiver
        .session
        .spawn_task(
            t1,
            Vec::new(),
            FinishableTask {
                started: Arc::clone(&t1_started),
                finish: tokio::sync::Mutex::new(Some(finish_rx)),
            },
        )
        .await;
    timeout(Duration::from_secs(10), t1_started.notified())
        .await
        .expect("T1 started");
    let (validated_tx, validated_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .scoped_steer_submission_gate
        .lock()
        .expect("submission gate") = Some((validated_tx, proceed_rx));
    let sending_control = control.clone();
    let sending = tokio::spawn(async move {
        sending_control
            .send_scoped_agent_input_observing_response(
                source,
                SENDER_TURN,
                target.thread_id,
                text_input("must never be injected into T2"),
                TurnStartOptions::default(),
                ResponseObservationPolicy::from_turn_parts(
                    /*commentary*/ false,
                    FinalResponseObservation::None,
                    /*target_messages*/ false,
                    /*queue_input*/ false,
                ),
            )
            .await
    });
    timeout(Duration::from_secs(10), validated_rx)
        .await
        .expect("T1 authorization validated")
        .expect("validation signal");
    // No permission/lifecycle fencing: natural task completion and a new session
    // turn remain possible while the authorized steer is paused.
    finish_tx.send(()).expect("finish T1");
    timeout(Duration::from_secs(10), async {
        loop {
            if matches!(receiver.next_event().await.expect("receiver event").msg,
                EventMsg::TurnComplete(event) if event.turn_id == T1)
            {
                break;
            }
        }
    })
    .await
    .expect("T1 completed");
    let t2_started = Arc::new(tokio::sync::Notify::new());
    let t2 = receiver
        .session
        .new_turn_with_default_settings(T2.to_string(), Default::default())
        .await;
    receiver
        .session
        .spawn_task(
            t2,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Regular,
                turn_start_gate: None,
                turn_start_attempted: Some(Arc::clone(&t2_started)),
            },
        )
        .await;
    timeout(Duration::from_secs(10), t2_started.notified())
        .await
        .expect("T2 started");
    assert!(receiver.session.get_pending_input().await.is_empty());
    proceed_tx.send(()).expect("submit validated T1 steer");
    let error = timeout(Duration::from_secs(10), sending)
        .await
        .expect("submission resolves")
        .expect("send task")
        .expect_err("T2 must reject T1 steer");
    let reason = codex_protocol::turn_input::NotSubmittedReason::ExpectedTurnMismatch {
        expected: T1.to_string(),
        actual: T2.to_string(),
    };
    assert_eq!(
        error.to_string(),
        CodexErr::InvalidRequest(format!("turn input was not submitted: {reason:?}"),).to_string()
    );
    assert_eq!(
        receiver.session.active_agent_response_turn_id(),
        Some(T2.to_string())
    );
    assert!(
        receiver.session.get_pending_input().await.is_empty(),
        "no input injected into T2"
    );
    sender.shutdown_and_wait().await.expect("shutdown sender");
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[test_case(Delivery::Wake, Revocation::Directed; "immediate_wake_directed_disable")]
#[test_case(Delivery::Steer, Revocation::Directed; "immediate_steer_directed_disable")]
#[test_case(Delivery::Wake, Revocation::Subtree; "immediate_wake_subtree_disable")]
#[test_case(Delivery::Steer, Revocation::Subtree; "immediate_steer_subtree_disable")]
#[tokio::test]
async fn immediate_scoped_admission_rechecks_permission_after_waits(
    delivery: Delivery,
    revocation: Revocation,
) {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = root.session.services.agent_control.clone();
    let sender = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn sender");
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn receiver");
    let sender = harness
        .manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender runtime");
    let receiver = harness
        .manager
        .get_thread(receiver.target_thread_id)
        .await
        .expect("receiver runtime");
    root.set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await
        .expect("enable peer sends");

    // Real active tasks establish the same admission snapshots as running model turns,
    // without making provider requests or relying on scheduler delays.
    let sender_started = Arc::new(tokio::sync::Notify::new());
    let sender_turn = sender
        .session
        .new_turn_with_default_settings("scoped-admission-sender".to_string(), Default::default())
        .await;
    let sender_turn_id = sender_turn.sub_id.clone();
    sender
        .session
        .spawn_task(
            sender_turn,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Regular,
                turn_start_gate: None,
                turn_start_attempted: Some(Arc::clone(&sender_started)),
            },
        )
        .await;
    timeout(Duration::from_secs(10), sender_started.notified())
        .await
        .expect("sender started");
    if matches!(delivery, Delivery::Steer) {
        let receiver_started = Arc::new(tokio::sync::Notify::new());
        let receiver_turn = receiver
            .session
            .new_turn_with_default_settings(
                "scoped-admission-receiver".to_string(),
                Default::default(),
            )
            .await;
        receiver
            .session
            .spawn_task(
                receiver_turn,
                Vec::new(),
                BlockingTask {
                    kind: TaskKind::Regular,
                    turn_start_gate: None,
                    turn_start_attempted: Some(Arc::clone(&receiver_started)),
                },
            )
            .await;
        timeout(Duration::from_secs(10), receiver_started.notified())
            .await
            .expect("receiver started");
    }
    let receiver_turn_before = receiver.session.active_agent_response_turn_id();
    assert_eq!(
        receiver_turn_before.is_some(),
        matches!(delivery, Delivery::Steer)
    );
    let source = sender.session.presentation_id();
    let target = receiver.session.presentation_id();
    let captured_before = harness.manager.captured_ops().len();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .scoped_permission_check_gate
        .lock()
        .expect("scoped permission gate") = Some((reached_tx, proceed_rx));
    let sending_control = control.clone();
    let sending = tokio::spawn(async move {
        sending_control
            .send_scoped_agent_input_observing_response(
                source,
                &sender_turn_id,
                target.thread_id,
                text_input("must not pass revoked scoped admission"),
                TurnStartOptions::default(),
                ResponseObservationPolicy::from_turn_parts(
                    /*commentary*/ false,
                    FinalResponseObservation::None,
                    /*target_messages*/ false,
                    /*queue_input*/ false,
                ),
            )
            .await
    });
    timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("common admission gate reached")
        .expect("selected scoped admission");
    // Permission has been selected, but exact admission has not acquired the shared
    // transaction. Commit the revocation before releasing that admission.
    timeout(Duration::from_secs(10), async {
        match revocation {
            Revocation::Directed => {
                control
                    .replace_durable_target_message_route(
                        source.thread_id,
                        target.thread_id,
                        TargetMessageRouteMode::Disabled,
                    )
                    .await
                    .expect("commit directed disable");
            }
            Revocation::Subtree => {
                root.set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
                    .await
                    .expect("commit subtree disable");
            }
        }
    })
    .await
    .expect("revocation does not need the receiver lifecycle");
    proceed_tx.send(()).expect("resume selected admission");
    let error = timeout(Duration::from_secs(10), sending)
        .await
        .expect("admission resolves")
        .expect("send task")
        .expect_err("revoked scope must not be submitted");
    assert_eq!(
        error.to_string(),
        CodexErr::InvalidRequest(
            "agent message permission was revoked or its scoped grant expired".to_string(),
        )
        .to_string()
    );
    assert_eq!(harness.manager.captured_ops().len(), captured_before);
    assert_eq!(
        receiver.session.active_agent_response_turn_id(),
        receiver_turn_before
    );
    assert_eq!(
        control.target_message_route_mode(target, source),
        Some(TargetMessageRouteMode::Disabled)
    );
    sender.shutdown_and_wait().await.expect("shutdown sender");
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}
