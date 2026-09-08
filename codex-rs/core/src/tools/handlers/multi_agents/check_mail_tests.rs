use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::registry::ToolExposure;
use crate::tools::registry::ToolRegistry;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn direct_dispatch_carries_selection_without_consuming_and_nested_dispatch_is_rejected()
-> anyhow::Result<()> {
    let (session, mut turn) = make_session_and_context().await;
    turn.multi_agent_version = MultiAgentVersion::V1;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let registry = ToolRegistry::with_handler_for_test(Arc::new(Handler));
    let tool_name = Handler.tool_name();
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "direct-mailbox".to_string(),
        tool_name: tool_name.clone(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: r#"{"from":"user"}"#.to_string(),
        },
    };
    assert_eq!(
        registry.tool_exposure(&tool_name),
        Some(ToolExposure::DirectModelOnly),
    );
    let direct = registry
        .dispatch_any_with_terminal_outcome(
            invocation.clone(),
            /*terminal_outcome_reached*/ None,
        )
        .await?
        .into_direct_result();
    let operation = direct
        .mailbox_operation
        .expect("accepted result carries selection");
    assert_eq!(
        (operation.tool_call_id, operation.selection),
        (
            "direct-mailbox".to_string(),
            MailboxSelection::Senders(vec![MailboxSender::User])
        ),
    );
    let ResponseItem::FunctionCallOutput { output, .. } = direct.response.item else {
        anyhow::bail!("direct mailbox response must be a tool result");
    };
    assert_eq!(
        serde_json::from_str::<JsonValue>(output.text_content().expect("metadata text"))?,
        serde_json::json!({"status": "delivery_requested", "from": "user"}),
    );
    let error = registry
        .dispatch_any_with_terminal_outcome(
            ToolInvocation {
                source: ToolCallSource::CodeMode {
                    cell_id: "cell".to_string(),
                    runtime_tool_call_id: "nested-mail".to_string(),
                },
                ..invocation
            },
            /*terminal_outcome_reached*/ None,
        )
        .await
        .err()
        .expect("nested mailbox call must fail without an ordered outer effect");
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "check_mail is disabled in nested/code-mode execution until the outer result supports ordered delivery".to_string(),
        ),
    );
    Ok(())
}
