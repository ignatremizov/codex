use anyhow::Context;
use anyhow::Result;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentObservationBinding;
use codex_core::UserAgentObservationMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn explicit_observer_changes_only_the_reverse_existing_subscription() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        let server = responses::start_mock_server().await;
        let test = test_codex()
            .with_config(move |config| {
                config
                    .features
                    .enable(Feature::Collab)
                    .expect("enable agents");
                if multi_agent_v2 {
                    config
                        .features
                        .enable(Feature::MultiAgentV2)
                        .expect("enable V2");
                } else {
                    config
                        .features
                        .disable(Feature::MultiAgentV2)
                        .expect("disable V2");
                }
            })
            .build_with_auto_env(&server)
            .await?;
        let root_id = test.session_configured.thread_id;
        let spawned = test
            .codex
            .spawn_agent(UserAgentSpawnOptions {
                response_handling: UserAgentResponseHandling::Wake,
                ..Default::default()
            })
            .await?;
        let child_id = spawned.target_thread_id;
        let child_selector = child_id.to_string();
        let error = test
            .codex
            .observe_agent(
                "Main",
                Some(&child_selector),
                UserAgentObservationMode::Presentation,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no active or pending response observation")
        );

        let child = test.thread_manager.get_thread(child_id).await?;
        child
            .resume_agent("Main", /*task*/ None, UserAgentResponseHandling::Wake)
            .await?;
        let original_statuses = (test.codex.agent_status().await, child.agent_status().await);
        assert_eq!(
            test.codex
                .observe_agent(
                    "Main",
                    Some(&child_selector),
                    UserAgentObservationMode::Presentation,
                )
                .await?,
            (
                root_id,
                child_id,
                UserAgentFinalResponseHandling::Wake,
                UserAgentObservationBinding::NextTurn
            )
        );
        // Read the effective reverse policy through another replacement, issued by its owner.
        assert_eq!(
            child
                .observe_agent(
                    "Main",
                    /*observer*/ None,
                    UserAgentObservationMode::Passive
                )
                .await?,
            (
                root_id,
                child_id,
                UserAgentFinalResponseHandling::Presentation,
                UserAgentObservationBinding::NextTurn
            )
        );
        assert_eq!(
            test.codex
                .observe_agent(
                    &child_selector,
                    /*observer*/ None,
                    UserAgentObservationMode::Wake,
                )
                .await?,
            (
                child_id,
                root_id,
                UserAgentFinalResponseHandling::Wake,
                UserAgentObservationBinding::NextTurn
            )
        );
        assert_eq!(
            original_statuses,
            (test.codex.agent_status().await, child.agent_status().await)
        );
        let requests = server.received_requests().await.context("mock requests")?;
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().ends_with("/responses"))
        );
    }
    Ok(())
}
