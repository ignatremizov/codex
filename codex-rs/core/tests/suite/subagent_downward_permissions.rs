use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "non_paginated")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downward_disable_is_rejected_without_changing_task_dispatch(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
        .await?;
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let error = test
        .codex
        .set_agent_reply_route(
            "Main",
            Some(&child_id.to_string()),
            UserAgentReplyRouteMode::Disabled,
        )
        .await
        .expect_err("supervisor task dispatch cannot be disabled");
    assert!(
        error
            .to_string()
            .contains("cannot disable supervisor send_input to a descendant"),
        "{error}"
    );
    // Rejection must not install a misleading disabled route.
    assert_eq!(
        test.codex
            .set_agent_reply_route(
                "Main",
                Some(&child_id.to_string()),
                UserAgentReplyRouteMode::Enabled,
            )
            .await?
            .2,
        None
    );
    Ok(())
}
