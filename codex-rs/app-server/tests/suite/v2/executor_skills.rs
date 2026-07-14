use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::CapabilityRootLocation;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SelectedCapabilityRoot;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const EXECUTOR_SKILL_NAME: &str = "demo-plugin:deploy";
const EXECUTOR_SKILL_MARKER: &str = "EXECUTOR_SKILL_BODY_MARKER";
const LOCAL_SKILL_MARKER: &str = "LOCAL_SKILL_BODY_MARKER";
const GOAL_SKILL_NAME: &str = "guidance";

#[tokio::test]
async fn selected_executor_root_injects_only_the_current_turn_body() -> Result<()> {
    // TODO(anp): Remove after selected capability-root fixtures can be materialized in remote exec.
    skip_if_remote!(
        Ok(()),
        "selected capability root fixture is only materialized on the host"
    );

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-selected"),
                responses::ev_assistant_message("msg-selected", "Done"),
                responses::ev_completed("resp-selected"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-follow-up"),
                responses::ev_assistant_message("msg-follow-up", "Done"),
                responses::ev_completed("resp-follow-up"),
            ])),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    write_skill_test_config(codex_home.path(), &server.uri())?;
    let local_skill_dir = codex_home.path().join("skills/local-deploy");
    std::fs::create_dir_all(&local_skill_dir)?;
    std::fs::write(
        local_skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {EXECUTOR_SKILL_NAME}\ndescription: Colliding local skill.\n---\n\n\
             # Local deploy\n\n{LOCAL_SKILL_MARKER}\n"
        ),
    )?;
    let plugin_dir = TempDir::new()?;
    let manifest_dir = plugin_dir.path().join(".codex-plugin");
    let skill_dir = plugin_dir.path().join("skills/deploy");
    std::fs::create_dir_all(&manifest_dir)?;
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        manifest_dir.join("plugin.json"),
        r#"{"name":"demo-plugin"}"#,
    )?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: deploy\ndescription: Deploy through the executor.\n---\n\n\
             # Deploy\n\n{EXECUTOR_SKILL_MARKER}\n"
        ),
    )?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let request_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            selected_capability_roots: Some(vec![SelectedCapabilityRoot {
                id: "demo-plugin@1".to_string(),
                location: CapabilityRootLocation::Environment {
                    environment_id: "local".to_string(),
                    path: PathUri::from_host_native_path(plugin_dir.path())?,
                },
            }]),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(response)?;

    timeout(
        READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: format!("Use ${EXECUTOR_SKILL_NAME}"),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    timeout(
        READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: "Continue without selecting another skill.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;

    let requests = response_mock.requests();
    assert_eq!(2, requests.len());
    let first_request = &requests[0];
    let developer_context = first_request.message_input_texts("developer");
    assert!(
        developer_context
            .iter()
            .all(|text| !text.contains(EXECUTOR_SKILL_MARKER))
    );
    let promoted_inventories = developer_context
        .iter()
        .filter(|text| text.contains("<promoted_skills>"))
        .collect::<Vec<_>>();
    assert!(!promoted_inventories.is_empty());
    assert!(
        promoted_inventories
            .iter()
            .all(|text| text.contains("<promoted_skills>[]</promoted_skills>")),
        "executor selections without a durable read route must not be promoted"
    );
    let skill_fragments = first_request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(1, skill_fragments.len());
    let skill_fragment = &skill_fragments[0];
    assert!(skill_fragment.contains(&format!("<name>{EXECUTOR_SKILL_NAME}</name>")));
    assert!(skill_fragment.contains(EXECUTOR_SKILL_MARKER));
    assert!(!skill_fragment.contains(LOCAL_SKILL_MARKER));

    let follow_up = &requests[1];
    assert!(
        follow_up
            .message_input_texts("developer")
            .iter()
            .all(|text| !text.contains(EXECUTOR_SKILL_MARKER))
    );
    let retained_skill_fragments = follow_up
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<skill>"))
        .count();
    assert_eq!(
        1, retained_skill_fragments,
        "the executor exception must not append a second carried-forward body"
    );

    Ok(())
}

#[tokio::test]
async fn goal_selected_hidden_host_skill_is_promoted_on_first_continuation() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("goal-continuation"),
            responses::ev_assistant_message("goal-response", "Done"),
            responses::ev_completed_with_tokens("goal-continuation", /*total_tokens*/ 200),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_skill_test_config(codex_home.path(), &server.uri())?;
    let config_path = codex_home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        &config_path,
        config.replace(
            "model_auto_compact_token_limit = 200000\n",
            "model_auto_compact_token_limit = 200000\n[features]\ngoals = true\n",
        ),
    )?;
    let skill_dir = codex_home.path().join("skills").join(GOAL_SKILL_NAME);
    std::fs::create_dir_all(skill_dir.join("agents"))?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_path,
        format!(
            "---\nname: {GOAL_SKILL_NAME}\ndescription: Explicit-only goal guidance.\n---\n\n\
             # Guidance\n"
        ),
    )?;
    std::fs::write(
        skill_dir.join("agents/openai.yaml"),
        "policy:\n  allow_implicit_invocation: false\n",
    )?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let request_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(response)?;

    let goal_id = app_server
        .send_raw_request(
            "thread/goal/set",
            Some(json!({
                "threadId": thread.id,
                "objective": "Use the explicitly selected guidance skill.",
                "skills": [{
                    "name": GOAL_SKILL_NAME,
                    "path": skill_path.to_string_lossy(),
                }],
                "tokenBudget": 100,
            })),
        )
        .await?;
    let response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(goal_id)),
    )
    .await??;
    let _: ThreadGoalSetResponse = to_response(response)?;
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request = response_mock.single_request();
    let developer_context = request.message_input_texts("developer");
    assert!(
        developer_context.iter().any(|text| {
            text.contains("<promoted_skills>")
                && text.contains(GOAL_SKILL_NAME)
                && text.contains(&skill_path.to_string_lossy().replace('\\', "/"))
        }),
        "the first goal continuation should expose its selected hidden host skill"
    );

    Ok(())
}

fn write_skill_test_config(codex_home: &std::path::Path, server_uri: &str) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "mock_provider"
compact_prompt = "Summarize the conversation."
model_auto_compact_token_limit = 200000

[skills]
include_instructions = true

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )?;
    Ok(())
}
