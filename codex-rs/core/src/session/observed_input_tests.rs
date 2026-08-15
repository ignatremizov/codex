use super::*;
use crate::session::command_approval::QueuedSubmission;
use crate::session::session_loop_termination_from_handle;
use crate::session::tests::make_session_and_context;
use codex_protocol::protocol::AgentStatus;
use futures::poll;
use pretty_assertions::assert_eq;
use std::sync::Arc;

async fn fixture() -> (
    Arc<Session>,
    SessionIo,
    async_channel::Receiver<QueuedSubmission>,
) {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    let (_, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: tokio::sync::watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(tokio::spawn(async {})),
    };
    (session, io, rx_sub)
}

#[tokio::test]
async fn idle_rejection_is_not_an_admitted_or_uncertain_input() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartIfIdle,
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { mode, reply, .. } = queued.op else {
        panic!("turn input")
    };
    assert_eq!(mode, TurnInputMode::StartIfIdle);
    reply
        .send(Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle,
        }))
        .unwrap();
    assert!(matches!(
        submission.await.unwrap(),
        ObservedTurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle
        }
    ));
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn accepted_input_keeps_actual_turn_when_observation_receipt_fails() {
    for routing in [
        TurnInputSubmission::Started {
            turn_id: "actual-turn".to_string(),
        },
        TurnInputSubmission::Steered {
            turn_id: "actual-turn".to_string(),
        },
    ] {
        let (session, io, submissions) = fixture().await;
        let mut submission = Box::pin(io.submit_observed_turn_input(
            &session,
            TurnInputRequest::user_input(Vec::new()),
            TurnInputMode::StartOrSteer,
        ));
        assert!(poll!(&mut submission).is_pending());
        let queued = submissions.recv().await.unwrap().submission;
        let Op::TurnInput { reply, .. } = queued.op else {
            panic!("turn input")
        };
        reply.send(Ok(routing)).unwrap();
        session.reject_input_turn_admission(&queued.id, CodexErr::InternalAgentDied);
        let ObservedTurnInputSubmission::AdmittedWithoutObservation {
            submission_id,
            target_turn_id,
            warning,
        } = submission.await.unwrap()
        else {
            panic!("accepted with warning")
        };
        assert_eq!(
            (submission_id, target_turn_id),
            (queued.id, "actual-turn".to_string())
        );
        assert!(warning.contains("do not resubmit"));
        assert!(session.submission_admission.requires_reload());
    }
}

#[tokio::test]
async fn routing_loss_after_enqueue_is_not_proof_of_non_admission() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartOrSteer,
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let id = queued.id.clone();
    drop(queued);
    let ObservedTurnInputSubmission::Indeterminate {
        submission_id,
        warning,
    } = submission.await.unwrap()
    else {
        panic!("unknown routing")
    };
    assert_eq!(submission_id, id);
    assert!(warning.contains("do not resubmit"));
    assert!(session.submission_admission.requires_reload());
}

#[tokio::test]
async fn cancelled_waiter_cannot_change_queued_idle_only_mode() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartIfIdle,
    ));
    assert!(poll!(&mut submission).is_pending());
    drop(submission);
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { mode, .. } = queued.op else {
        panic!("turn input")
    };
    assert_eq!(mode, TurnInputMode::StartIfIdle);
}

#[tokio::test]
async fn strict_adapter_rejects_uncertain_input_and_quarantines() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_turn_input_with_admission(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartOrSteer,
    ));
    assert!(poll!(&mut submission).is_pending());
    drop(submissions.recv().await.unwrap());
    assert!(submission.await.is_err());
    assert!(session.submission_admission.requires_reload());
}

fn queue_metadata() -> AgentQueueTurnMetadata {
    AgentQueueTurnMetadata {
        queue_id: uuid::Uuid::now_v7().to_string(),
        source_thread_id: codex_protocol::ThreadId::new(),
        response_handling: Some(codex_protocol::protocol::AgentQueueResponseHandling {
            commentary: true,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            target_messages: true,
        }),
    }
}

#[tokio::test]
async fn queued_admission_returns_before_start_permission_and_prompt_persistence() {
    let (session, io, submissions) = fixture().await;
    let metadata = queue_metadata();
    let mut submission = Box::pin(io.submit_observed_queued_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        metadata.clone(),
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { mode, reply, .. } = queued.op else {
        panic!("turn input")
    };
    assert_eq!(mode, TurnInputMode::StartIfIdle);
    let resolution = session.capture_input_turn_admission_resolution("actual-turn".to_string());
    session.resolve_input_turn_admission(&queued.id, resolution);
    reply
        .send(Ok(TurnInputSubmission::Started {
            turn_id: "actual-turn".to_string(),
        }))
        .unwrap();
    let ObservedTurnInputSubmission::Admitted {
        resolution,
        queue_start_permit: Some(permit),
        input_persisted: Some(mut persisted),
        ..
    } = submission.await.unwrap()
    else {
        panic!("admitted queued input")
    };
    assert_eq!(resolution.target_turn_id, "actual-turn");
    let mut startup = Box::pin(session.await_agent_queue_turn_metadata("actual-turn"));
    assert!(poll!(&mut startup).is_pending());
    assert!(poll!(&mut persisted).is_pending());
    permit.publish();
    assert_eq!(startup.await, Some(metadata));
    assert!(poll!(&mut persisted).is_pending());
    session.settle_queued_input_persistence("actual-turn", Ok(()));
    persisted.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_queued_waiter_preserves_admission_but_not_uncommitted_response_policy() {
    let (session, io, submissions) = fixture().await;
    let mut metadata = queue_metadata();
    let mut submission = Box::pin(io.submit_observed_queued_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        metadata.clone(),
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    drop(submission);
    let resolution = session.capture_input_turn_admission_resolution("actual-turn".to_string());
    session.resolve_input_turn_admission(&queued.id, resolution);
    metadata.response_handling = None;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.await_agent_queue_turn_metadata("actual-turn")
        )
        .await
        .unwrap(),
        Some(metadata),
    );
    session.settle_queued_input_persistence("actual-turn", Ok(()));
}

#[tokio::test]
async fn queued_routing_error_after_enqueue_is_unknown_not_retryable_rejection() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_queued_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        queue_metadata(),
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { reply, .. } = queued.op else {
        panic!("turn input")
    };
    reply.send(Err(CodexErr::InternalAgentDied)).unwrap();
    assert!(matches!(
        submission.await.unwrap(),
        ObservedTurnInputSubmission::Indeterminate { .. }
    ));
    assert!(session.submission_admission.requires_reload());
}

#[tokio::test]
async fn queued_startup_survives_abort_grace_and_preserves_accepted_steer() {
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::user_input::UserInput;
    use std::time::Duration;
    use tokio::time::timeout;

    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let metadata = queue_metadata();
    let (permit, persisted) = session.register_queued_input_start("queued", metadata.clone());
    let resolution = session.capture_input_turn_admission_resolution(turn.sub_id.clone());
    session.resolve_input_turn_admission("queued", resolution);
    let queued_prompt = vec![UserInput::Text {
        text: "queued task".to_string(),
        text_elements: Vec::new(),
    }];
    let steered_prompt = vec![UserInput::Text {
        text: "steered task".to_string(),
        text_elements: Vec::new(),
    }];
    session
        .spawn_task(
            Arc::clone(&turn),
            vec![crate::session::TurnInput::UserInput {
                content: queued_prompt.clone(),
                client_id: None,
                acceptance_order: None,
            }],
            crate::tasks::RegularTask::new(),
        )
        .await;
    assert_eq!(
        crate::session::turn_input::handle(
            &session,
            TurnInputRequest::user_input(steered_prompt.clone()),
            TurnInputMode::Steer {
                expected_turn_id: turn.sub_id.clone()
            },
            "steer".to_string(),
        )
        .await
        .unwrap(),
        TurnInputSubmission::Steered {
            turn_id: turn.sub_id.clone()
        },
    );
    let mut abort = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.abort_all_tasks(TurnAbortReason::Interrupted).await }
    });
    assert!(
        timeout(Duration::from_millis(150), &mut abort)
            .await
            .is_err()
    );
    permit.publish();
    timeout(Duration::from_secs(5), abort)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(5), persisted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (queues, prompts, aborted) = timeout(Duration::from_secs(5), async {
        let mut queues = Vec::new();
        let mut prompts = Vec::new();
        let mut aborted = false;
        while prompts.len() < 2 {
            match events.recv().await.unwrap().msg {
                EventMsg::TurnStarted(event) => queues.push(event.agent_queue),
                EventMsg::ItemCompleted(event) => {
                    if let TurnItem::UserMessage(item) = event.item {
                        prompts.push(item.content);
                    }
                }
                EventMsg::TurnAborted(_) => aborted = true,
                _ => {}
            }
        }
        (queues, prompts, aborted)
    })
    .await
    .unwrap();
    assert_eq!(
        (queues, prompts, aborted),
        (
            vec![Some(metadata), None],
            vec![queued_prompt, steered_prompt],
            true
        ),
    );
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
}
