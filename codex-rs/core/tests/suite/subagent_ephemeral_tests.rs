//! Ephemeral agents retain live completion behavior without canonical persistence.

use super::*;
use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(MultiAgentVersion::V1; "v1")]
#[test_case(MultiAgentVersion::V2; "v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_spawn_preserves_live_completion_and_followup_context(
    version: MultiAgentVersion,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const ROOT_PROMPT: &str = "spawn an ephemeral worker";
    const TASK: &str = "finish the ephemeral assignment";
    const RESULT: &str = "ephemeral child conclusion";
    const FOLLOWUP: &str = "Continue after the ephemeral completion.";
    let server = start_mock_server().await;
    let (model, namespace, arguments) = match version {
        MultiAgentVersion::V1 => (
            INHERITED_MODEL,
            MULTI_AGENT_V1_NAMESPACE,
            json!({"message": TASK}),
        ),
        MultiAgentVersion::V2 => (
            "koffing",
            MULTI_AGENT_V2_NAMESPACE,
            json!({"message": TASK, "task_name": "worker", "fork_turns": "none"}),
        ),
        MultiAgentVersion::Disabled => unreachable!("multi-agent fixture"),
    };
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, ROOT_PROMPT) && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("ephemeral-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                namespace,
                "spawn_agent",
                &arguments.to_string(),
            ),
            ev_completed("ephemeral-spawn"),
        ]),
    )
    .await;
    let child_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, TASK) && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("ephemeral-child"),
            ev_assistant_message("ephemeral-child-final", RESULT),
            ev_completed("ephemeral-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SPAWN_CALL_ID) && !body_contains(request, FOLLOWUP)
        },
        sse(vec![
            ev_response_created("ephemeral-parent"),
            ev_assistant_message("ephemeral-parent-final", "parent done"),
            ev_completed("ephemeral-parent"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model(model)
        .with_config(move |config| {
            config.ephemeral = true;
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collaboration");
            match version {
                MultiAgentVersion::V1 => config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("V1 fixture should disable V2"),
                MultiAgentVersion::V2 => config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("V2 fixture should enable V2"),
                MultiAgentVersion::Disabled => unreachable!("multi-agent fixture"),
            };
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Plaintext;
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: ROOT_PROMPT.to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    // Child publication can precede or follow the real parent terminal event. Do not discard
    // either event while waiting for the other or infer completion from a parent-only receipt.
    let (_, status, reference, text) = timeout(Duration::from_secs(/*secs*/ 15), async {
        let mut parent_done = false;
        let mut completion = None;
        loop {
            let event = test.codex.next_event().await?;
            if let Some(item) = sub_agent_completion_event(&event.msg) {
                completion = Some(item);
            }
            match event.msg {
                EventMsg::TurnComplete(event)
                    if event.last_agent_message.as_deref() == Some("parent done") =>
                {
                    parent_done = true;
                }
                EventMsg::Error(error) => {
                    anyhow::bail!("ephemeral completion failed: {}", error.message)
                }
                _ => {}
            }
            if parent_done && let Some(completion) = completion.take() {
                break Ok::<_, anyhow::Error>(completion);
            }
        }
    })
    .await??;
    assert_eq!(
        (status, text),
        (SubAgentCompletionStatus::Completed, RESULT.to_owned())
    );
    let child_id = ThreadId::from_string(&wait_for_spawned_thread_id(&test).await?)?;
    let child = test.thread_manager.get_thread(child_id).await?;
    assert_eq!(
        reference,
        match version {
            MultiAgentVersion::V1 => child_id.to_string(),
            MultiAgentVersion::V2 => "/root/worker".to_string(),
            MultiAgentVersion::Disabled => unreachable!("multi-agent fixture"),
        }
    );
    assert!(child.config_snapshot().await.ephemeral);
    assert_eq!(
        wait_for_terminal_status(&child).await?,
        AgentStatus::Completed(Some(RESULT.to_owned()))
    );
    assert_eq!(
        wait_for_terminal_status(&test.codex).await?,
        AgentStatus::Completed(Some("parent done".to_owned()))
    );
    assert!(test.codex.rollout_path().is_none());
    assert!(child.rollout_path().is_none());
    let child_requests = wait_for_requests(&child_response).await?;
    assert!(
        child_requests
            .iter()
            .any(|request| request.body_contains_text(TASK)
                && !request.body_contains_text(SPAWN_CALL_ID))
    );

    if version == MultiAgentVersion::V2 {
        // Queue-only V2 mail need not be consumed before the first followup sampling request.
        mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                body_contains(request, FOLLOWUP) && !body_contains(request, RESULT)
            },
            sse(vec![
                ev_response_created("ephemeral-wait"),
                ev_function_call_with_namespace(
                    "ephemeral-wait-call",
                    namespace,
                    "wait_agent",
                    "{}",
                ),
                ev_completed("ephemeral-wait"),
            ]),
        )
        .await;
    }
    let followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FOLLOWUP) && body_contains(request, RESULT)
        },
        sse(vec![
            ev_response_created("ephemeral-root-followup"),
            ev_assistant_message("ephemeral-root-followup-final", "finished"),
            ev_completed("ephemeral-root-followup"),
        ]),
    )
    .await;
    test.submit_turn(FOLLOWUP).await?;
    let request = wait_for_request_containing_text(&followup, RESULT).await?;
    match version {
        MultiAgentVersion::V1 => assert!(has_subagent_notification(&request)),
        MultiAgentVersion::V2 => assert!(
            request
                .inputs_of_type("agent_message")
                .iter()
                .any(|item| { item["author"] == "/root/worker" && item["recipient"] == "/root" })
        ),
        MultiAgentVersion::Disabled => unreachable!("multi-agent fixture"),
    }
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
