use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::turn_input::NotSubmittedReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[path = "user_shell_canonical_tests.rs"]
mod canonical_tests;

struct ContinuingTask;

impl SessionTask for ContinuingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.shell_delivery_test"
    }

    fn supports_pending_input_continuation(&self) -> bool {
        true
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        std::future::pending().await
    }
}

fn result_item() -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "shell result".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn wake_steers_only_an_invocation_that_can_still_continue() {
    for accepting in [true, false] {
        let (session, turn_context, _events) = make_session_and_context_with_rx().await;
        session
            .spawn_task(Arc::clone(&turn_context), Vec::new(), ContinuingTask)
            .await;
        let turn_state = {
            let active = session.active_turn.lock().await;
            let active = active.as_ref().expect("active turn");
            active
                .task
                .as_ref()
                .expect("running task")
                .accepting_pending_input
                .store(accepting, Ordering::Release);
            Arc::clone(&active.turn_state)
        };
        let item = result_item();
        let completion = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("shell result owner");
        assert_eq!(
            session
                .deliver_user_shell_result(
                    item.clone(),
                    &turn_context,
                    UserShellCommandFinalDelivery::Wake,
                    &completion,
                )
                .await
                .expect("accept result"),
            !accepting,
        );
        let expected_input = vec![TurnInput::UserShellContextReady];
        assert_eq!(
            session
                .input_queue
                .take_pending_input_for_turn_state(&turn_state)
                .await,
            if accepting {
                expected_input.clone()
            } else {
                Vec::new()
            },
        );
        assert_eq!(
            session.input_queue.take_queued_items_for_next_turn().await,
            if accepting {
                (Vec::new(), TurnStartOptions::default())
            } else {
                (
                    expected_input,
                    TurnStartOptions {
                        turn_trigger: Some("user_shell_wake".to_string()),
                        ..Default::default()
                    },
                )
            },
        );
    }
}

#[tokio::test]
async fn passive_taskless_result_uses_canonical_history_and_presentation_only_does_not() {
    for delivery in [
        UserShellCommandFinalDelivery::Passive,
        UserShellCommandFinalDelivery::PresentationOnly,
    ] {
        let (session, turn_context, _events) = make_session_and_context_with_rx().await;
        let active = ActiveTurn::default();
        let state = Arc::clone(&active.turn_state);
        *session.active_turn.lock().await = Some(active);
        let completion = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("shell result owner");
        session
            .deliver_user_shell_result(result_item(), &turn_context, delivery, &completion)
            .await
            .expect("deliver");
        assert_eq!(
            session
                .input_queue
                .take_pending_input_for_turn_state(&state)
                .await,
            Vec::new()
        );
        let items = session.clone_history().await.into_annotated_items();
        if delivery == UserShellCommandFinalDelivery::Passive {
            assert_eq!(items.len(), 1);
            let mut item = items[0].item.clone();
            item.set_id(None);
            item.clear_internal_chat_message_metadata_passthrough();
            assert_eq!(item, result_item());
        } else {
            assert_eq!(items, Vec::new());
        }
    }
}

#[tokio::test]
async fn queued_shell_wake_is_admission_guarded_and_blocks_automatic_idle_work() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    let submission_id = session
        .services
        .unified_exec_manager
        .reserve_user_shell_submission(UserShellCommandFinalDelivery::Wake)
        .await;
    let outcome = crate::session::turn_input::handle(
        &session,
        TurnInputRequest::new(SubmittedTurnInput::ResponseItem(result_item())),
        TurnInputMode::StartIfIdle,
        "automatic-idle".to_string(),
    )
    .await
    .expect("idle decision");
    assert_eq!(
        outcome,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        }
    );
    assert!(session.active_turn.lock().await.is_none());
    session
        .services
        .unified_exec_manager
        .release_user_shell_submission(submission_id)
        .await;
    let completion = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("shell accepted before quarantine");
    session.submission_admission.rollback_requires_reload();
    assert!(
        session
            .deliver_user_shell_result(
                result_item(),
                &turn_context,
                UserShellCommandFinalDelivery::Wake,
                &completion,
            )
            .await
            .is_err()
    );
    assert_eq!(
        session.input_queue.take_queued_items_for_next_turn().await,
        (Vec::new(), TurnStartOptions::default())
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        Vec::new()
    );
}

#[tokio::test]
async fn one_shot_shell_wake_records_history_without_starting_another_turn() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    session
        .set_app_server_client_info(
            Some("codex_exec".to_string()),
            /*app_server_client_version*/ None,
            /*mcp_elicitations_auto_deny*/ false,
        )
        .await
        .expect("set one-shot client");
    let completion = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("shell result owner");
    assert!(
        !session
            .deliver_user_shell_result(
                result_item(),
                &turn_context,
                UserShellCommandFinalDelivery::Wake,
                &completion,
            )
            .await
            .expect("deliver shell result")
    );
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        session.input_queue.take_queued_items_for_next_turn().await,
        (Vec::new(), TurnStartOptions::default())
    );
    let items = session.clone_history().await.into_annotated_items();
    assert_eq!(items.len(), 1);
    let mut item = items[0].item.clone();
    item.set_id(None);
    item.clear_internal_chat_message_metadata_passthrough();
    assert_eq!(item, result_item());
}

#[tokio::test]
async fn accepted_shell_result_after_shutdown_is_canonical_without_a_new_wake() {
    for delivery in [
        UserShellCommandFinalDelivery::Passive,
        UserShellCommandFinalDelivery::Wake,
        UserShellCommandFinalDelivery::PresentationOnly,
    ] {
        let (session, turn, _events) = make_session_and_context_with_rx().await;
        session
            .spawn_task(Arc::clone(&turn), Vec::new(), ContinuingTask)
            .await;
        let state = {
            let active = session.active_turn.lock().await;
            Arc::clone(&active.as_ref().expect("continuable turn").turn_state)
        };
        let completion = session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("command accepted before shutdown");
        session.submission_admission.close_completion_admission();
        assert!(
            session
                .submission_admission
                .admit_injection()
                .await
                .is_err()
        );
        assert!(
            session
                .submission_admission
                .try_accept_completion_delivery()
                .is_none()
        );
        assert!(
            !session
                .deliver_user_shell_result(result_item(), &turn, delivery, &completion)
                .await
                .expect("preaccepted result retains publication authority")
        );
        assert_eq!(
            session
                .input_queue
                .take_pending_input_for_turn_state(&state)
                .await,
            Vec::new()
        );
        assert_eq!(
            session.input_queue.take_queued_items_for_next_turn().await,
            (Vec::new(), TurnStartOptions::default())
        );
        let actual = session
            .clone_history()
            .await
            .into_annotated_items()
            .into_iter()
            .map(|envelope| {
                let mut item = envelope.item;
                item.set_id(None);
                item.clear_internal_chat_message_metadata_passthrough();
                item
            })
            .collect::<Vec<_>>();
        let expected = if delivery == UserShellCommandFinalDelivery::PresentationOnly {
            Vec::new()
        } else {
            vec![result_item()]
        };
        assert_eq!(actual, expected);
        drop(completion);
        tokio::time::timeout(
            std::time::Duration::from_secs(/*secs*/ 5),
            session.submission_admission.drain_accepted_completions(),
        )
        .await
        .expect("result publication released its shutdown receipt");
    }
}

#[tokio::test]
async fn another_session_cannot_supply_shell_completion_authority() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let foreign = Arc::new(crate::session::SubmissionAdmission::default());
    let completion = foreign
        .try_accept_completion_delivery()
        .expect("foreign receipt");
    assert!(
        session
            .deliver_user_shell_result(
                result_item(),
                &turn,
                UserShellCommandFinalDelivery::Passive,
                &completion,
            )
            .await
            .is_err()
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        Vec::new()
    );
    assert_eq!(
        session.input_queue.take_queued_items_for_next_turn().await,
        (Vec::new(), TurnStartOptions::default())
    );
}

#[tokio::test]
async fn sealed_unload_preserves_accepted_shell_context_without_admitting_a_wake() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let completion = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("shell accepted before unload");
    session.submission_admission.seal_for_unload();
    assert!(session.submission_admission.check_ready().is_err());
    assert!(
        !session.submission_admission.completion_is_closing(),
        "the unload seal precedes ordinary shutdown and is an independent admission fence"
    );
    assert!(
        !session
            .deliver_user_shell_result(
                result_item(),
                &turn,
                UserShellCommandFinalDelivery::Wake,
                &completion,
            )
            .await
            .expect("accepted context survives sealing")
    );
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        session.input_queue.take_queued_items_for_next_turn().await,
        (Vec::new(), TurnStartOptions::default())
    );
    let history = session.clone_history().await.into_annotated_items();
    assert_eq!(history.len(), 1);
    let mut item = history[0].item.clone();
    item.set_id(None);
    item.clear_internal_chat_message_metadata_passthrough();
    assert_eq!(item, result_item());
}
