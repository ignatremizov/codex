use super::super::start_command_execution_item;
use super::*;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;
use anyhow::Result;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use core_test_support::load_default_config_for_test;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::mpsc;

struct Fixture {
    _home: TempDir,
    _manager: codex_core::ThreadManager,
    conversation: Arc<CodexThread>,
    thread_id: ThreadId,
    state: Arc<Mutex<ThreadState>>,
    outgoing: ThreadScopedOutgoingMessageSender,
    receipt: CommandExecutionStartReceipt,
}

fn completion_item() -> CommandExecutionCompletionItem {
    CommandExecutionCompletionItem {
        model_context: None,
        plugin_id: None,
        script_path: None,
        command: "echo approval".into(),
        cwd: test_path_buf("/tmp").abs().into(),
        command_actions: Vec::new(),
    }
}

fn start_turn(state: &mut ThreadState, turn_id: &str) {
    state.track_current_turn_event(
        turn_id,
        &EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: turn_id.into(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        }),
    );
}

impl Fixture {
    async fn new(
        connections: Vec<ConnectionId>,
    ) -> Result<(Self, mpsc::Receiver<OutgoingEnvelope>)> {
        let home = TempDir::new()?;
        let config = load_default_config_for_test(&home).await;
        let manager = codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        );
        let thread = manager
            .start_thread(codex_core::StartThreadOptions::new(config))
            .await?;
        let mut state = ThreadState::default();
        state.listener_thread = Some(Arc::downgrade(&thread.thread));
        start_turn(&mut state, "turn");
        let state = Arc::new(Mutex::new(state));
        let (sender, receiver) = mpsc::channel(/*buffer*/ 1);
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            Arc::new(OutgoingMessageSender::new(
                sender,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            connections,
            thread.thread_id,
        );
        let origin = {
            let state = state.lock().await;
            CommandApprovalOrigin::capture(&state, &thread.thread, "turn")
                .expect("current event listener")
        };
        let receipt = start_root_command_execution_item(
            origin,
            &thread.thread_id,
            "command",
            &completion_item(),
            &outgoing,
            &state,
        )
        .await
        .expect("initial root start");
        Ok((
            Self {
                _home: home,
                _manager: manager,
                conversation: thread.thread,
                thread_id: thread.thread_id,
                state,
                outgoing,
                receipt,
            },
            receiver,
        ))
    }

    async fn complete(&self, receipt: Option<CommandExecutionStartReceipt>) {
        complete_command_execution_item(
            &self.thread_id,
            "turn".into(),
            "command".into(),
            completion_item(),
            /*process_id*/ None,
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Declined,
            Some(&self.conversation),
            receipt,
            &self.outgoing,
            &self.state,
        )
        .await;
    }
}

fn notification(envelope: OutgoingEnvelope) -> ServerNotification {
    let message = match envelope {
        OutgoingEnvelope::Broadcast { message }
        | OutgoingEnvelope::ToConnection { message, .. } => message,
    };
    let OutgoingMessage::AppServerNotification(envelope) = message else {
        panic!("expected server notification");
    };
    envelope.notification
}

#[tokio::test]
async fn receipt_publishes_once_and_cannot_complete_a_same_id_replacement() -> Result<()> {
    let (fixture, mut receiver) = Fixture::new(vec![ConnectionId(1)]).await?;
    receiver.try_recv().expect("start");
    let origin = {
        let state = fixture.state.lock().await;
        CommandApprovalOrigin::capture(&state, &fixture.conversation, "turn")
            .expect("same event listener")
    };
    assert!(
        start_root_command_execution_item(
            origin,
            &fixture.thread_id,
            "command",
            &completion_item(),
            &fixture.outgoing,
            &fixture.state,
        )
        .await
        .is_none(),
        "an existing command does not grant another approval ownership"
    );
    assert!(receiver.try_recv().is_err());
    fixture.complete(Some(fixture.receipt.clone())).await;
    let ServerNotification::ItemCompleted(completed) =
        notification(receiver.try_recv().expect("owned completion"))
    else {
        panic!("expected completion")
    };
    let item = completion_item();
    assert_eq!(
        completed.item,
        ThreadItem::CommandExecution {
            id: "command".into(),
            model_context: item.model_context,
            plugin_id: item.plugin_id,
            script_path: item.script_path,
            command: item.command,
            cwd: item.cwd,
            process_id: None,
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            status: CommandExecutionStatus::Declined,
            command_actions: item.command_actions,
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
        }
    );
    fixture.complete(Some(fixture.receipt.clone())).await;
    assert!(receiver.try_recv().is_err());

    let item = completion_item();
    let replacement = start_command_execution_item(
        &fixture.thread_id,
        "turn".into(),
        "command".into(),
        item.model_context,
        item.plugin_id,
        item.script_path,
        item.command,
        item.cwd,
        item.command_actions,
        CommandExecutionSource::Agent,
        &fixture.outgoing,
        &fixture.state,
    )
    .await
    .expect("replacement root start");
    receiver.try_recv().expect("replacement start");
    fixture.complete(Some(fixture.receipt.clone())).await;
    assert!(receiver.try_recv().is_err());
    {
        let state = fixture.state.lock().await;
        assert!(Arc::ptr_eq(
            &state.turn_summary.command_execution_receipts["command"].token,
            &replacement.token,
        ));
        assert!(
            state
                .turn_summary
                .command_execution_started
                .contains("command")
        );
    }
    fixture.complete(Some(replacement)).await;
    assert!(matches!(
        notification(receiver.try_recv().expect("replacement completion")),
        ServerNotification::ItemCompleted(_)
    ));
    fixture.conversation.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_completion_invalidates_the_approval_owned_receipt() -> Result<()> {
    let (fixture, mut receiver) = Fixture::new(vec![ConnectionId(1)]).await?;
    receiver.try_recv().expect("start");
    fixture.complete(/*receipt*/ None).await;
    assert!(matches!(
        notification(receiver.try_recv().expect("canonical completion")),
        ServerNotification::ItemCompleted(_)
    ));
    fixture.complete(Some(fixture.receipt.clone())).await;
    assert!(receiver.try_recv().is_err());
    fixture.conversation.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn root_start_rejects_listener_replacement_during_eligibility_lookup() -> Result<()> {
    let (fixture, mut receiver) = Fixture::new(vec![ConnectionId(1)]).await?;
    receiver.try_recv().expect("initial start");
    let origin = {
        let mut state = fixture.state.lock().await;
        state.turn_summary.command_execution_started.clear();
        state.turn_summary.command_execution_receipts.clear();
        let origin = CommandApprovalOrigin::capture(&state, &fixture.conversation, "turn")
            .expect("event origin before eligibility lookup");
        state.listener_generation += 1;
        origin
    };
    assert!(
        start_root_command_execution_item(
            origin,
            &fixture.thread_id,
            "command",
            &completion_item(),
            &fixture.outgoing,
            &fixture.state,
        )
        .await
        .is_none()
    );
    assert!(receiver.try_recv().is_err());
    {
        let state = fixture.state.lock().await;
        assert!(state.turn_summary.command_execution_started.is_empty());
        assert!(state.turn_summary.command_execution_receipts.is_empty());
    }
    fixture.conversation.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn root_start_rechecks_origin_after_output_backpressure_without_removing_a_replacement()
-> Result<()> {
    for replace_item in [false, true] {
        let (fixture, mut receiver) = Fixture::new(vec![ConnectionId(1)]).await?;
        let origin = {
            let mut state = fixture.state.lock().await;
            state.turn_summary.command_execution_started.clear();
            state.turn_summary.command_execution_receipts.clear();
            CommandApprovalOrigin::capture(&state, &fixture.conversation, "turn")
                .expect("current event listener")
        };
        let item = completion_item();
        // The earlier start still occupies the one output slot.
        let mut start = Box::pin(start_root_command_execution_item(
            origin,
            &fixture.thread_id,
            "command",
            &item,
            &fixture.outgoing,
            &fixture.state,
        ));
        assert!(futures::poll!(&mut start).is_pending());
        let replacement_token = Arc::new(());
        {
            let mut state = fixture.state.lock().await;
            assert!(
                state
                    .turn_summary
                    .command_execution_receipts
                    .contains_key("command")
            );
            state.listener_generation += 1;
            if replace_item {
                let mut replacement = fixture.receipt.clone();
                replacement.token = Arc::clone(&replacement_token);
                replacement.listener_generation = state.listener_generation;
                state
                    .turn_summary
                    .command_execution_receipts
                    .insert("command".into(), replacement);
            }
        }
        receiver.try_recv().expect("release output capacity");
        assert!(start.await.is_none());
        assert!(
            receiver.try_recv().is_err(),
            "old root must not emit a late start"
        );
        {
            let state = fixture.state.lock().await;
            if replace_item {
                assert!(Arc::ptr_eq(
                    &state.turn_summary.command_execution_receipts["command"].token,
                    &replacement_token,
                ));
                assert!(
                    state
                        .turn_summary
                        .command_execution_started
                        .contains("command")
                );
            } else {
                assert!(state.turn_summary.command_execution_receipts.is_empty());
                assert!(state.turn_summary.command_execution_started.is_empty());
            }
        }
        fixture.conversation.shutdown_and_wait().await?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Invalidation {
    ListenerGeneration,
    ListenerIdentity,
    Turn,
    CanonicalCompletion,
    SameIdReplacement,
    SameIdReplacementRemoved,
}

#[tokio::test]
async fn output_backpressure_revalidates_receipt_before_any_publication() -> Result<()> {
    for invalidation in [
        Invalidation::ListenerGeneration,
        Invalidation::ListenerIdentity,
        Invalidation::Turn,
        Invalidation::CanonicalCompletion,
        Invalidation::SameIdReplacement,
        Invalidation::SameIdReplacementRemoved,
    ] {
        let (fixture, mut receiver) = Fixture::new(vec![ConnectionId(1)]).await?;
        // The initial start occupies the only output slot. Polling reaches the actual
        // reservation wait deterministically, without sleeps or a production test hook.
        let mut completion = Box::pin(fixture.complete(Some(fixture.receipt.clone())));
        assert!(futures::poll!(&mut completion).is_pending());
        let mut duplicate = Box::pin(fixture.complete(Some(fixture.receipt.clone())));
        assert!(
            futures::poll!(&mut duplicate).is_ready(),
            "publishing claim is single-use"
        );
        drop(duplicate);
        let replacement_token = Arc::new(());
        {
            // Obtaining this lock also proves the blocked send does not retain ThreadState.
            let mut state = fixture.state.lock().await;
            assert!(
                state.turn_summary.command_execution_receipts["command"].completion
                    == CommandExecutionCompletionState::Publishing
            );
            match invalidation {
                Invalidation::ListenerGeneration => state.listener_generation += 1,
                Invalidation::ListenerIdentity => state.listener_thread = None,
                Invalidation::Turn => start_turn(&mut state, "replacement-turn"),
                Invalidation::CanonicalCompletion => {
                    state
                        .turn_summary
                        .command_execution_started
                        .remove("command");
                    state
                        .turn_summary
                        .command_execution_receipts
                        .remove("command");
                }
                Invalidation::SameIdReplacement | Invalidation::SameIdReplacementRemoved => {
                    let mut replacement = fixture.receipt.clone();
                    replacement.token = Arc::clone(&replacement_token);
                    state
                        .turn_summary
                        .command_execution_receipts
                        .insert("command".into(), replacement);
                    if matches!(invalidation, Invalidation::SameIdReplacementRemoved) {
                        state
                            .turn_summary
                            .command_execution_started
                            .remove("command");
                        state
                            .turn_summary
                            .command_execution_receipts
                            .remove("command");
                    }
                }
            }
        }
        receiver.try_recv().expect("release output capacity");
        completion.await;
        assert!(
            receiver.try_recv().is_err(),
            "stale completion must not be published"
        );
        {
            let state = fixture.state.lock().await;
            if matches!(invalidation, Invalidation::SameIdReplacement) {
                assert!(Arc::ptr_eq(
                    &state.turn_summary.command_execution_receipts["command"].token,
                    &replacement_token,
                ));
                assert!(
                    state
                        .turn_summary
                        .command_execution_started
                        .contains("command")
                );
            } else {
                assert!(
                    !state
                        .turn_summary
                        .command_execution_receipts
                        .contains_key("command")
                );
            }
        }
        fixture.conversation.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unobserved_or_closed_output_retires_only_the_owned_receipt() -> Result<()> {
    for connections in [Vec::new(), vec![ConnectionId(1)]] {
        let (fixture, receiver) = Fixture::new(connections).await?;
        drop(receiver);
        fixture.complete(Some(fixture.receipt.clone())).await;
        let state = fixture.state.lock().await;
        assert!(state.turn_summary.command_execution_receipts.is_empty());
        assert!(state.turn_summary.command_execution_started.is_empty());
        drop(state);
        fixture.conversation.shutdown_and_wait().await?;
    }
    Ok(())
}
