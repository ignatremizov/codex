//! Public submission must reject forged receiver identity before turn admission.

use anyhow::Result;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::AgentPath;
use codex_protocol::PreparedAgentInput;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::InterAgentCommunication;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "wrong receiver id")]
#[test_case(true; "wrong receiver incarnation")]
#[tokio::test]
async fn prepared_agent_input_public_submission_rejects_foreign_receiver(
    current_receiver: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let input = PreparedAgentInput {
        communication: InterAgentCommunication::new_encrypted(
            AgentPath::try_from("/root/worker").map_err(anyhow::Error::msg)?,
            AgentPath::root(),
            Vec::new(),
            "opaque unadmitted input".to_string(),
            /*trigger_turn*/ true,
        ),
        attribution: Box::new(AgentInputAttribution {
            sender: identity(ThreadId::new()),
            recipient: identity(if current_receiver {
                test.session_configured.thread_id
            } else {
                ThreadId::new()
            }),
            sender_turn_id: "unadmitted-source".to_string(),
            batch_id: None,
        }),
        sender_instance_id: uuid::Uuid::now_v7(),
        recipient_instance_id: uuid::Uuid::now_v7(),
        call_id: "unadmitted-invocation".to_string(),
    };
    let error = test
        .codex
        .start_or_steer_turn(TurnInputRequest::new(TurnInput::PreparedAgentInput(
            Box::new(input),
        )))
        .await
        .expect_err("foreign prepared input must be rejected");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert!(error.to_string().contains(if current_receiver {
        "previous receiver incarnation"
    } else {
        "does not match its endpoints"
    }));
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        0,
        "rejection precedes sampling"
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
