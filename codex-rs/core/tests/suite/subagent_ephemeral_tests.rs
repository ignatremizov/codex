//! Ephemeral agents retain live completion behavior without canonical persistence.

use super::*;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(MultiAgentVersion::V1, UserAgentResponseHandling::Passive; "v1 passive")]
#[test_case(MultiAgentVersion::V1, UserAgentResponseHandling::Presentation; "v1 presentation")]
#[test_case(MultiAgentVersion::V2, UserAgentResponseHandling::Passive; "v2 passive")]
#[test_case(MultiAgentVersion::V2, UserAgentResponseHandling::Presentation; "v2 presentation")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_spawn_delivers_only_the_requested_completion_context(
    version: MultiAgentVersion,
    response_handling: UserAgentResponseHandling,
) -> Result<()> {
    const TASK: &str = "finish the ephemeral assignment";
    const RESULT: &str = "ephemeral child conclusion";
    let server = start_mock_server().await;
    let child_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, TASK),
        sse(vec![
            ev_response_created("ephemeral-child"),
            ev_assistant_message("ephemeral-child-final", RESULT),
            ev_completed("ephemeral-child"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(move |config| {
            config.ephemeral = true;
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should enable agent controls");
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
        })
        .build_with_auto_env(&server)
        .await?;
    let root_status = test.codex.agent_status().await;
    let spawned = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            input: Some(vec![UserInput::Text {
                text: TASK.to_owned(),
                text_elements: Vec::new(),
            }]),
            response_handling,
            ..Default::default()
        })
        .await?;
    assert_eq!(spawned.input_outcome, Some(UserAgentInputOutcome::Admitted));
    assert_eq!(spawned.post_admission_warning, None);
    let child = test
        .thread_manager
        .get_thread(spawned.target_thread_id)
        .await?;
    assert!(child.config_snapshot().await.ephemeral);
    assert_eq!(
        wait_for_terminal_status(&child).await?,
        AgentStatus::Completed(Some(RESULT.to_owned())),
    );
    let (item_id, status, _, text) = timeout(Duration::from_secs(/*secs*/ 15), async {
        loop {
            let event = test.codex.next_event().await?;
            if let Some(completion) = sub_agent_completion_event(&event.msg) {
                break Ok::<_, anyhow::Error>(completion);
            }
            if let EventMsg::Error(error) = event.msg {
                anyhow::bail!("ephemeral completion failed: {}", error.message);
            }
        }
    })
    .await??;
    assert_eq!(
        (status, text),
        (SubAgentCompletionStatus::Completed, RESULT.to_owned())
    );
    let expected_visibility = if response_handling == UserAgentResponseHandling::Passive {
        SubAgentCompletionModelVisibility::Visible
    } else {
        SubAgentCompletionModelVisibility::NotVisible
    };
    assert_eq!(
        sub_agent_completion_model_visibility_from_response_item_id(&item_id),
        Some(expected_visibility),
    );
    assert_eq!(test.codex.agent_status().await, root_status);
    assert!(test.codex.rollout_path().is_none());
    assert!(child.rollout_path().is_none());
    child_response.single_request();

    let followup =
        responses::mount_sse_once(&server, responses::sse_completed("ephemeral-root")).await;
    test.submit_text_turn("Continue after the ephemeral completion.")
        .await?;
    assert_eq!(
        followup.single_request().body_contains_text(RESULT),
        response_handling == UserAgentResponseHandling::Passive,
    );
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
