use super::super::Handler;
use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::user_input::UserInput;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::ClaimMailboxInputParams;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use codex_tools::ToolExecutor;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

async fn accept_agent_mail(session: &Session, sender: ThreadId) -> anyhow::Result<()> {
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    session
        .services
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: session.thread_id,
            submission_key: uuid::Uuid::now_v7().to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Selected sender mail".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(session.thread_id),
                    sender_turn_id: "sender-turn".to_string(),
                }),
            },
        })
        .await?;
    Ok(())
}

fn direct_wait_invocation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    targets: Vec<String>,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "wait-selected".to_string(),
        tool_name: Handler::default().tool_name(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({"targets": targets}).to_string(),
        },
    }
}

#[tokio::test]
async fn activity_rechecks_selection_without_claiming_or_consuming() -> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    let selected = ThreadId::new();
    let unrelated = ThreadId::new();
    let selection = MailboxSelection::Senders(vec![MailboxSender::Agent(selected)]);
    let wait = wait_for_outcome(
        &session,
        Vec::new(),
        Vec::new(),
        Instant::now() + Duration::from_secs(/*secs*/ 10),
        Some(&selection),
    );
    tokio::pin!(wait);
    assert!(wait.as_mut().now_or_never().is_none());

    accept_agent_mail(&session, unrelated).await?;
    session.notify_mailbox_activity();
    assert!(wait.as_mut().now_or_never().is_none());
    session
        .services
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: session.thread_id,
            submission_key: "user-mail".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "Unselected user mail".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await?;
    session.notify_mailbox_activity();
    assert!(wait.as_mut().now_or_never().is_none());

    accept_agent_mail(&session, selected).await?;
    let before = session
        .services
        .thread_store
        .read_mailbox_inventory(session.thread_id)
        .await?;
    session.notify_mailbox_activity();
    let (statuses, reason) = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), wait).await??;
    assert_eq!((statuses, reason), (Vec::new(), WaitReturnReason::Mail));
    assert_eq!(
        session
            .services
            .thread_store
            .read_mailbox_inventory(session.thread_id)
            .await?,
        before,
    );
    Ok(())
}

#[tokio::test]
async fn pending_mail_needs_no_activity_signal_and_completion_wins() -> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    let sender = ThreadId::new();
    accept_agent_mail(&session, sender).await?;
    let selection = MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]);
    let (statuses, reason) = wait_for_outcome(
        &session,
        Vec::new(),
        Vec::new(),
        Instant::now(),
        Some(&selection),
    )
    .await?;
    assert_eq!((statuses, reason), (Vec::new(), WaitReturnReason::Mail));
    let (statuses, reason) = wait_for_outcome(
        &session,
        Vec::new(),
        vec![(
            sender,
            TerminalStatusEvent {
                turn_id: Some("completed-turn".to_string()),
                status: AgentStatus::Completed(Some("done".to_string())),
            },
        )],
        Instant::now(),
        Some(&selection),
    )
    .await?;
    assert_eq!(
        (statuses, reason),
        (
            vec![(
                sender,
                TerminalStatusEvent {
                    turn_id: Some("completed-turn".to_string()),
                    status: AgentStatus::Completed(Some("done".to_string())),
                }
            )],
            WaitReturnReason::Completion
        ),
    );
    Ok(())
}

#[tokio::test]
async fn unselected_pending_mail_does_not_prevent_timeout() -> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    accept_agent_mail(&session, ThreadId::new()).await?;
    let selection = MailboxSelection::Senders(vec![MailboxSender::Agent(ThreadId::new())]);
    let (statuses, reason) = wait_for_outcome(
        &session,
        Vec::new(),
        Vec::new(),
        Instant::now(),
        Some(&selection),
    )
    .await?;
    assert_eq!((statuses, reason), (Vec::new(), WaitReturnReason::Timeout));
    Ok(())
}

#[tokio::test]
async fn ready_terminal_subscription_wins_over_selected_mail() -> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    let (sender, _) = make_session_and_context().await;
    let (_, subscription) = sender.subscribe_terminal_status();
    accept_agent_mail(&session, sender.thread_id).await?;
    sender.prepare_for_thread_removal();
    let (expected_terminal, _) = sender.subscribe_terminal_status();
    let selection = MailboxSelection::Senders(vec![MailboxSender::Agent(sender.thread_id)]);
    let (statuses, reason) = wait_for_outcome(
        &session,
        vec![(sender.thread_id, subscription)],
        Vec::new(),
        Instant::now(),
        Some(&selection),
    )
    .await?;
    assert_eq!(
        (statuses, reason),
        (
            vec![(sender.thread_id, expected_terminal)],
            WaitReturnReason::Completion,
        ),
    );
    Ok(())
}

#[tokio::test]
async fn direct_result_carries_operation_but_nested_retains_completion_only_resolution()
-> anyhow::Result<()> {
    let (mut session, turn) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let sender = ThreadId::new();
    accept_agent_mail(&session, sender).await?;
    let handler = Handler::default();
    let invocation = direct_wait_invocation(
        Arc::clone(&session),
        turn,
        vec![sender.to_string(), format!("id:{sender}")],
    );
    let before = session
        .services
        .thread_store
        .read_mailbox_inventory(session.thread_id)
        .await?;
    let (output, operation) = handler
        .handle_with_mailbox_operation(invocation.clone())
        .await?;
    let operation = operation.expect("direct accepted result must carry selection");
    assert_eq!(
        (
            output.code_mode_result(&invocation.payload),
            operation.tool_call_id,
            operation.selection
        ),
        (
            serde_json::json!({
                "status": {},
                "timed_out": false,
                "return_reason": "mail",
                "mail_only_targets": [sender.to_string()],
            }),
            invocation.call_id.clone(),
            MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]),
        ),
    );
    let nested = ToolInvocation {
        source: ToolCallSource::CodeMode {
            cell_id: "cell".to_string(),
            runtime_tool_call_id: "nested-wait".to_string(),
        },
        // Existing unknown raw UUID completion behavior is NotFound; forced-ID
        // error behavior is not broadened into mail-only selection either.
        payload: ToolPayload::Function {
            arguments: serde_json::json!({"targets": [sender.to_string()]}).to_string(),
        },
        ..invocation
    };
    let (output, operation) = handler
        .handle_with_mailbox_operation(nested.clone())
        .await?;
    assert!(operation.is_none());
    assert_eq!(
        output.code_mode_result(&nested.payload),
        serde_json::json!({
            "status": {sender.to_string(): AgentStatus::NotFound},
            "timed_out": false,
            "return_reason": "completion",
        }),
    );
    assert!(
        handler
            .handle_with_mailbox_operation(ToolInvocation {
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({"targets": [format!("id:{sender}")]}).to_string(),
                },
                ..nested
            })
            .await
            .is_err()
    );
    assert_eq!(
        session
            .services
            .thread_store
            .read_mailbox_inventory(session.thread_id)
            .await?,
        before,
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum ClaimContents {
    Empty,
    WithMail,
}

#[test_case(ClaimContents::Empty; "empty fixed claim")]
#[test_case(ClaimContents::WithMail; "claimed mail no longer pending")]
#[tokio::test]
async fn same_invocation_recovers_immediately_without_expanding_fixed_membership(
    contents: ClaimContents,
) -> anyhow::Result<()> {
    let (mut session, turn) = make_session_and_context().await;
    attach_in_memory_thread_store(&mut session).await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let sender = ThreadId::new();
    if matches!(contents, ClaimContents::WithMail) {
        accept_agent_mail(&session, sender).await?;
    }
    let invocation = direct_wait_invocation(
        Arc::clone(&session),
        Arc::clone(&turn),
        vec![sender.to_string()],
    );
    let params = ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id: session.thread_id,
            turn_id: turn.sub_id.clone(),
            tool_call_id: invocation.call_id.clone(),
        },
        selection: MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]),
    };
    let claim = session
        .services
        .thread_store
        .claim_mailbox_input(params.clone())
        .await?;
    let handler = Handler::default();
    let (output, operation) = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 2),
        handler.handle_with_mailbox_operation(invocation.clone()),
    )
    .await??;
    let operation = operation.expect("recovery carries the same ordered operation");
    let expected_output = serde_json::json!({
        "status": {},
        "timed_out": false,
        "return_reason": "recovery",
        "mail_only_targets": [sender.to_string()],
    });
    assert_eq!(
        (
            output.code_mode_result(&invocation.payload),
            operation.tool_call_id,
            operation.selection
        ),
        (
            expected_output.clone(),
            invocation.call_id.clone(),
            params.selection.clone()
        ),
    );

    accept_agent_mail(&session, sender).await?;
    let before = session
        .services
        .thread_store
        .read_mailbox_inventory(session.thread_id)
        .await?;
    let (output, _) = handler
        .handle_with_mailbox_operation(invocation.clone())
        .await?;
    assert_eq!(
        output.code_mode_result(&invocation.payload),
        expected_output
    );
    assert_eq!(
        session
            .services
            .thread_store
            .recover_mailbox_delivery(params)
            .await?
            .claim,
        claim,
    );
    assert_eq!(
        session
            .services
            .thread_store
            .read_mailbox_inventory(session.thread_id)
            .await?,
        before,
    );

    let error = handler
        .handle_with_mailbox_operation(ToolInvocation {
            payload: ToolPayload::Function {
                arguments: serde_json::json!({"targets": [ThreadId::new().to_string()]})
                    .to_string(),
            },
            ..invocation
        })
        .await
        .err()
        .expect("same invocation cannot replace its sender selection");
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "wait_agent invocation already has a different fixed mailbox selection".to_string(),
        ),
    );
    Ok(())
}
