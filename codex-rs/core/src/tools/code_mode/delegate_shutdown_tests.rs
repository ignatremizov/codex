use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeToolKind;
use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::CodeModeCellDelegate;
use super::CodeModeDispatchBroker;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;

#[tokio::test]
async fn accepted_notification_remains_owned_after_its_waiter_disappears() {
    let (session, turn) = make_session_and_context().await;
    let broker = Arc::new(CodeModeDispatchBroker::new(
        session.thread_id,
        /*executed_tool_calls*/ Default::default(),
    ));
    let delegate = CodeModeCellDelegate {
        broker: Arc::clone(&broker),
        step_context: StepContext::for_test(Arc::new(turn)),
    };
    let mut callback = delegate.notify(
        "call".to_string(),
        CellId::new("cell".to_string()),
        "notice".to_string(),
        CancellationToken::new(),
    );
    assert!(futures::poll!(&mut callback).is_pending());
    let accepted = broker
        .dispatch_rx
        .recv()
        .await
        .expect("accepted notification");
    drop(callback);
    broker.begin_durable_shutdown();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            broker.wait_for_accepted_dispatch()
        )
        .await
        .is_err()
    );
    drop(accepted);
    assert_eq!(broker.wait_for_accepted_dispatch().await, Ok(()));
    assert_eq!(
        delegate
            .notify(
                "late".to_string(),
                CellId::new("cell".to_string()),
                "late".to_string(),
                CancellationToken::new(),
            )
            .await,
        Err("code mode notification dispatcher is shutting down".to_string())
    );
}

#[tokio::test]
async fn accepted_invocation_remains_owned_after_its_waiter_disappears() {
    let (session, turn) = make_session_and_context().await;
    let broker = Arc::new(CodeModeDispatchBroker::new(
        session.thread_id,
        /*executed_tool_calls*/ Default::default(),
    ));
    let delegate = CodeModeCellDelegate {
        broker: Arc::clone(&broker),
        step_context: StepContext::for_test(Arc::new(turn)),
    };
    let invocation = CodeModeNestedToolCall {
        cell_id: CellId::new("cell".to_string()),
        runtime_tool_call_id: "nested".to_string(),
        tool_name: ToolName::plain("example"),
        tool_kind: CodeModeToolKind::Function,
        input: None,
    };
    let mut callback = delegate.invoke_tool(invocation.clone(), CancellationToken::new());
    assert!(futures::poll!(&mut callback).is_pending());
    let accepted = broker
        .dispatch_rx
        .recv()
        .await
        .expect("accepted invocation");
    drop(callback);
    broker.begin_durable_shutdown();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            broker.wait_for_accepted_dispatch()
        )
        .await
        .is_err()
    );
    drop(accepted);
    assert_eq!(broker.wait_for_accepted_dispatch().await, Ok(()));
    assert_eq!(
        delegate
            .invoke_tool(invocation, CancellationToken::new())
            .await,
        Err("code mode nested tool dispatcher is shutting down".to_string())
    );
}
