//! Native spawn policies run before admission across configurable tool namespaces.

use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[derive(Clone, Copy)]
enum SpawnSurface {
    Fork,
    V2Default,
    V2Custom,
}

#[tokio::test]
#[test_case(SpawnSurface::Fork, "^Agent$"; "fork_agent_alias")]
#[test_case(SpawnSurface::Fork, "^spawn_agent$"; "fork_native_name")]
#[test_case(SpawnSurface::V2Default, "^Agent$"; "v2_agent_alias")]
#[test_case(SpawnSurface::V2Default, "^spawn_agent$"; "v2_native_name")]
#[test_case(SpawnSurface::V2Custom, "^Agent$"; "custom_agent_alias")]
#[test_case(SpawnSurface::V2Custom, "^spawn_agent$"; "custom_native_name")]
async fn pre_tool_use_denial_prevents_native_spawn(
    surface: SpawnSurface,
    matcher: &'static str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let namespace = match surface {
        SpawnSurface::Fork => "multi_agent_v1",
        SpawnSurface::V2Default => "collaboration",
        SpawnSurface::V2Custom => "custom_agents",
    };
    let args = match surface {
        SpawnSurface::Fork => json!({ "message": "inspect", "fork_context": true }),
        SpawnSurface::V2Default | SpawnSurface::V2Custom => {
            json!({ "message": "inspect", "task_name": "worker", "fork_turns": "all" })
        }
    };
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spawn"),
                responses::ev_function_call_with_namespace(
                    "spawn-call",
                    namespace,
                    "spawn_agent",
                    &args.to_string(),
                ),
                ev_completed("spawn"),
            ]),
            responses::sse_completed("denied"),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            write_pre_tool_use_hook(home, Some(matcher), "json_deny", "spawn denied")
                .expect("write spawn denial fixture");
        })
        .with_config(move |config| {
            trust_discovered_hooks(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("enable native agents");
            config.agent_allow_history_forks = true;
            if !matches!(surface, SpawnSurface::Fork) {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable V2");
                config.multi_agent_v2.non_code_mode_only = false;
                config.multi_agent_v2.tool_namespace = Some(namespace.to_string());
            }
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn("Start an agent with inherited history.")
        .await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output("spawn-call");
    assert!(
        output["output"]
            .as_str()
            .is_some_and(|text| text.contains("spawn denied")),
        "{output}"
    );
    let hooks = read_pre_tool_use_hook_inputs(test.codex_home_path())?;
    assert_eq!(
        hooks
            .iter()
            .map(|hook| (&hook["tool_name"], &hook["tool_use_id"]))
            .collect::<Vec<_>>(),
        vec![(&json!("spawn_agent"), &json!("spawn-call"))]
    );
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    Ok(())
}
