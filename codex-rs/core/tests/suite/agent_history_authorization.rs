//! Model-authored history inheritance is fresh by default and authorized after role resolution.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::AgentRoleConfig;
use codex_core::config::MultiAgentMessageDelivery;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::time::timeout;

const OLD_CONTEXT: &str = "older parent history sentinel";
const PARENT_PROMPT: &str = "delegate the history authorization task";
const CHILD_PROMPT: &str = "perform the isolated child assignment";
const CALL_ID: &str = "spawn-history-authorization";
const ROLE_INSTRUCTIONS: &str = "role-authorized child instructions";
const RESTORED_PROMPT: &str = "delegate again after restoring the default-role child";
const RESTORED_CALL: &str = "restored-child-history-fork";
const GRANDCHILD_TASK: &str = "perform the post-restore inherited-history task";
const ROOT_RESUME_PROMPT: &str = "restore the existing history-policy child";
const ROOT_RESUME_CALL: &str = "resume-history-policy-child";

#[derive(Clone, Copy)]
enum Version {
    V1,
    V2,
}

#[derive(Clone, Copy)]
enum History {
    Omitted,
    None,
    Full,
    LastTurn,
    ConfiguredFull,
}

#[derive(Clone, Copy)]
enum Authorization {
    Default,
    Global,
    RoleAllows,
    RoleDenies,
    DefaultRoleAllows,
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    let body = match request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
    {
        Some("zstd") => zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok(),
        _ => Some(request.body.clone()),
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

async fn wait_for_child_request(
    log: &responses::ResponseMock,
    prompt: &str,
    spawn_call_id: &str,
) -> Result<responses::ResponsesRequest> {
    Ok(timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = log.requests().into_iter().find(|request| {
                request.body_contains_text(prompt) && !request.body_contains_text(spawn_call_id)
            }) {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?)
}

#[test_case(Version::V1, History::Omitted, Authorization::Default, ThreadHistoryMode::Legacy; "v1 fresh legacy")]
#[test_case(Version::V2, History::Omitted, Authorization::Default, ThreadHistoryMode::Paginated; "v2 fresh paginated")]
#[test_case(Version::V1, History::Full, Authorization::Default, ThreadHistoryMode::Legacy; "v1 denied")]
#[test_case(Version::V2, History::Full, Authorization::Default, ThreadHistoryMode::Paginated; "v2 full denied")]
#[test_case(Version::V2, History::LastTurn, Authorization::Default, ThreadHistoryMode::Legacy; "v2 bounded denied")]
#[test_case(Version::V1, History::Full, Authorization::Global, ThreadHistoryMode::Paginated; "v1 global full")]
#[test_case(Version::V2, History::Full, Authorization::Global, ThreadHistoryMode::Legacy; "v2 global full")]
#[test_case(Version::V2, History::LastTurn, Authorization::Global, ThreadHistoryMode::Paginated; "v2 global bounded")]
#[test_case(Version::V1, History::Full, Authorization::RoleAllows, ThreadHistoryMode::Legacy; "v1 role allows")]
#[test_case(Version::V2, History::Full, Authorization::RoleAllows, ThreadHistoryMode::Paginated; "v2 role allows")]
#[test_case(Version::V2, History::LastTurn, Authorization::RoleAllows, ThreadHistoryMode::Legacy; "v2 bounded role allows")]
#[test_case(Version::V1, History::Full, Authorization::RoleDenies, ThreadHistoryMode::Paginated; "v1 role denies global")]
#[test_case(Version::V1, History::Omitted, Authorization::RoleDenies, ThreadHistoryMode::Legacy; "v1 restored role denies global")]
#[test_case(Version::V2, History::LastTurn, Authorization::RoleDenies, ThreadHistoryMode::Legacy; "v2 role denies global")]
#[test_case(Version::V1, History::Full, Authorization::DefaultRoleAllows, ThreadHistoryMode::Legacy; "v1 default role persisted")]
#[test_case(Version::V2, History::Full, Authorization::DefaultRoleAllows, ThreadHistoryMode::Paginated; "v2 default role persisted")]
#[test_case(Version::V2, History::ConfiguredFull, Authorization::Global, ThreadHistoryMode::Legacy; "v2 configured full")]
#[test_case(Version::V2, History::ConfiguredFull, Authorization::Default, ThreadHistoryMode::Paginated; "configured history does not authorize")]
#[test_case(Version::V2, History::Omitted, Authorization::RoleDenies, ThreadHistoryMode::Legacy; "role denial still allows fresh")]
#[test_case(Version::V2, History::None, Authorization::Default, ThreadHistoryMode::Paginated; "explicit none beats configured full")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_history_obeys_resolved_authorization(
    version: Version,
    history: History,
    authorization: Authorization,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let role_name = match authorization {
        Authorization::Default | Authorization::Global => None,
        Authorization::RoleAllows | Authorization::RoleDenies => Some("history-role"),
        Authorization::DefaultRoleAllows => Some("default"),
    };
    let mut builder = test_codex()
        .with_model("gpt-5.5")
        .with_history_mode(history_mode)
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("collab");
            // Cold-restore cases must reach the history gate when spawning a grandchild.
            config.agent_max_depth = 2;
            match version {
                Version::V1 => config.features.disable(Feature::MultiAgentV2),
                Version::V2 => config.features.enable(Feature::MultiAgentV2),
            }
            .expect("multi-agent version");
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::EncryptedWithAudit;
            config.agent_allow_history_forks = matches!(
                authorization,
                Authorization::Global | Authorization::RoleDenies
            );
            if matches!(history, History::ConfiguredFull | History::None) {
                config.multi_agent_v2.default_fork_turns = "all".to_string();
            }
            config.developer_instructions = Some("parent developer instructions".to_string());
            if let Some(role_name) = role_name {
                let role_path = config.codex_home.join("history-role.toml");
                let allowed = !matches!(authorization, Authorization::RoleDenies);
                std::fs::write(
                    &role_path,
                    format!(
                        "model = \"gpt-5.5\"\n\
                         model_reasoning_effort = \"high\"\n\
                         developer_instructions = \"{ROLE_INSTRUCTIONS}\"\n\
                         model_provider = \"ollama\"\n\
                         [agents]\nallow_history_forks = {allowed}\n"
                    ),
                )
                .expect("write user-authored role");
                config.agent_roles.insert(
                    role_name.to_string(),
                    AgentRoleConfig {
                        description: Some("Authorized history role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
            }
        });
    let test = builder.build_with_auto_env(&server).await?;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("old-turn"),
            responses::ev_assistant_message("old-answer", "older task finished"),
            responses::ev_completed("old-turn"),
        ]),
    )
    .await;
    test.submit_text_turn(OLD_CONTEXT).await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    let mut arguments = json!({"message": CHILD_PROMPT});
    if matches!(version, Version::V2) {
        arguments["task_name"] = json!("history-worker");
        arguments["task_message"] = json!(CHILD_PROMPT);
    }
    match (version, history) {
        (Version::V1, History::Full) => arguments["fork_context"] = json!(true),
        (Version::V2, History::Full) => arguments["fork_turns"] = json!("all"),
        (Version::V2, History::LastTurn) => arguments["fork_turns"] = json!("1"),
        (Version::V2, History::None) => arguments["fork_turns"] = json!("none"),
        (
            Version::V1,
            History::Omitted | History::None | History::LastTurn | History::ConfiguredFull,
        )
        | (Version::V2, History::Omitted | History::ConfiguredFull) => {}
    }
    if let Some(role_name) = role_name.filter(|role_name| *role_name != "default") {
        arguments["agent_type"] = json!(role_name);
    }
    if role_name.is_some() {
        // The role must win without allowing its provider override.
        arguments["model"] = json!("gpt-5.6-luna");
        arguments["reasoning_effort"] = json!("low");
    }
    let spawn = match version {
        Version::V1 => responses::ev_function_call_with_namespace(
            CALL_ID,
            "multi_agent_v1",
            "spawn_agent",
            &arguments.to_string(),
        ),
        Version::V2 => responses::ev_function_call_with_namespace(
            CALL_ID,
            "collaboration",
            "spawn_agent",
            &arguments.to_string(),
        ),
    };
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PARENT_PROMPT)
                && !body_contains(request, CALL_ID)
                && !body_contains(request, CHILD_PROMPT)
        },
        responses::sse(vec![
            responses::ev_response_created("parent-spawn"),
            spawn,
            responses::ev_completed("parent-spawn"),
        ]),
    )
    .await;
    let child_log = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("child"),
            responses::ev_assistant_message("child-answer", "assignment complete"),
            responses::ev_completed("child"),
        ]),
    )
    .await;
    let parent_log = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("parent-finish"),
            responses::ev_assistant_message("parent-answer", "delegation handled"),
            responses::ev_completed("parent-finish"),
        ]),
    )
    .await;
    test.submit_text_turn(PARENT_PROMPT).await?;
    let inherits = matches!(
        history,
        History::Full | History::LastTurn | History::ConfiguredFull
    );
    let denied = inherits
        && matches!(
            authorization,
            Authorization::Default | Authorization::RoleDenies
        );
    let parent_request = parent_log
        .requests()
        .into_iter()
        .find(|request| request.function_call_output_text(CALL_ID).is_some())
        .expect("parent receives spawn result");
    let output = parent_request
        .function_call_output_text(CALL_ID)
        .expect("spawn output");
    if denied {
        assert!(output.contains("Parent-history forks are disabled by user configuration"));
        assert!(
            created.try_recv().is_err(),
            "denial must not create a child"
        );
        return Ok(());
    }
    assert!(!output.contains("Parent-history forks are disabled"));
    let child_request = wait_for_child_request(&child_log, CHILD_PROMPT, CALL_ID).await?;
    let user_texts = child_request.message_input_texts("user");
    assert_eq!(
        (
            user_texts.iter().any(|text| text.contains(OLD_CONTEXT)),
            user_texts.iter().any(|text| text.contains(PARENT_PROMPT)),
        ),
        (
            matches!(history, History::Full | History::ConfiguredFull),
            inherits,
        )
    );
    let child_id = timeout(Duration::from_secs(10), created.recv()).await??;
    let child = test.thread_manager.get_thread(child_id).await?;
    let snapshot = child.config_snapshot().await;
    assert_eq!(snapshot.model_provider_id, test.config.model_provider_id);
    assert_eq!(
        snapshot.session_source.get_agent_role().as_deref(),
        role_name
    );
    if role_name.is_some() {
        assert_eq!(
            (
                child_request.body_json()["model"].clone(),
                child_request.body_json()["reasoning"]["effort"].clone(),
            ),
            (json!("gpt-5.5"), json!("high"))
        );
        assert!(child_request.body_contains_text(ROLE_INSTRUCTIONS));
    } else {
        assert!(child_request.body_contains_text("parent developer instructions"));
    }
    if matches!(
        (version, authorization),
        (Version::V1 | Version::V2, Authorization::DefaultRoleAllows)
            | (Version::V1, Authorization::RoleDenies)
    ) {
        timeout(
            Duration::from_secs(10),
            wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))),
        )
        .await?;
        child.flush_rollout().await?;
        timeout(Duration::from_secs(10), child.shutdown_and_wait()).await??;
        test.thread_manager.remove_thread(&child_id).await;
        drop(child);
        assert!(test.thread_manager.get_thread(child_id).await.is_err());
        let restored_allows = matches!(authorization, Authorization::DefaultRoleAllows);
        // Owner and role disagree: neither inheritance from the owner nor a forgotten
        // role identity can satisfy the post-restore model-authored fork.
        assert_eq!(
            test.codex.config().await.agent_allow_history_forks,
            !restored_allows
        );
        match version {
            Version::V2 => {
                timeout(
                    Duration::from_secs(10),
                    test.thread_manager
                        .ensure_multi_agent_v2_child_loaded(child_id),
                )
                .await??;
            }
            Version::V1 => {
                responses::mount_sse_once_match(
                    &server,
                    |request: &wiremock::Request| {
                        body_contains(request, ROOT_RESUME_PROMPT)
                            && !body_contains(request, ROOT_RESUME_CALL)
                    },
                    responses::sse(vec![
                        responses::ev_response_created("root-resume"),
                        responses::ev_function_call_with_namespace(
                            ROOT_RESUME_CALL,
                            "multi_agent_v1",
                            "resume_agent",
                            &json!({"id": child_id.to_string()}).to_string(),
                        ),
                        responses::ev_completed("root-resume"),
                    ]),
                )
                .await;
                responses::mount_sse_once_match(
                    &server,
                    |request: &wiremock::Request| body_contains(request, ROOT_RESUME_CALL),
                    responses::sse(vec![
                        responses::ev_response_created("root-resumed"),
                        responses::ev_completed("root-resumed"),
                    ]),
                )
                .await;
                test.submit_text_turn(ROOT_RESUME_PROMPT).await?;
            }
        }
        let restored = test.thread_manager.get_thread(child_id).await?;
        assert_eq!(
            restored.config().await.agent_allow_history_forks,
            restored_allows
        );
        if matches!(version, Version::V1) {
            let parent_snapshot = test.codex.config_snapshot().await;
            let restored_snapshot = restored.config_snapshot().await;
            assert_eq!(restored_snapshot.cwd(), parent_snapshot.cwd());
            assert_eq!(
                (
                    restored_snapshot.model,
                    restored_snapshot.reasoning_effort,
                    restored_snapshot.reasoning_summary,
                    restored_snapshot.model_provider_id,
                    restored_snapshot.permission_profile,
                    restored_snapshot.approval_policy,
                    restored_snapshot.approvals_reviewer,
                    restored_snapshot.service_tier,
                ),
                (
                    parent_snapshot.model,
                    parent_snapshot.reasoning_effort,
                    parent_snapshot.reasoning_summary,
                    parent_snapshot.model_provider_id,
                    parent_snapshot.permission_profile,
                    parent_snapshot.approval_policy,
                    parent_snapshot.approvals_reviewer,
                    parent_snapshot.service_tier,
                ),
            );
        }
        let mut restored_args = json!({
            "message": GRANDCHILD_TASK,
            // This built-in role has no authorization override, so it must inherit the
            // permission recovered by cold reload rather than reapplying the saved role.
            "agent_type": "worker",
        });
        let namespace = match version {
            Version::V1 => {
                restored_args["fork_context"] = json!(true);
                "multi_agent_v1"
            }
            Version::V2 => {
                restored_args["task_name"] = json!("grandchild");
                restored_args["task_message"] = json!(GRANDCHILD_TASK);
                restored_args["fork_turns"] = json!("all");
                "collaboration"
            }
        };
        let mut grandchildren = test.thread_manager.subscribe_thread_created();
        responses::mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                body_contains(request, RESTORED_PROMPT)
                    && !body_contains(request, RESTORED_CALL)
                    && !body_contains(request, GRANDCHILD_TASK)
            },
            responses::sse(vec![
                responses::ev_response_created("restored-spawn"),
                responses::ev_function_call_with_namespace(
                    RESTORED_CALL,
                    namespace,
                    "spawn_agent",
                    &restored_args.to_string(),
                ),
                responses::ev_completed("restored-spawn"),
            ]),
        )
        .await;
        let grandchild_log = responses::mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                body_contains(request, GRANDCHILD_TASK) && !body_contains(request, RESTORED_CALL)
            },
            responses::sse(vec![
                responses::ev_response_created("grandchild"),
                responses::ev_completed("grandchild"),
            ]),
        )
        .await;
        let restored_log = responses::mount_sse_once_match(
            &server,
            |request: &wiremock::Request| body_contains(request, RESTORED_CALL),
            responses::sse(vec![
                responses::ev_response_created("restored-finish"),
                responses::ev_completed("restored-finish"),
            ]),
        )
        .await;
        restored
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: RESTORED_PROMPT.to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        timeout(
            Duration::from_secs(10),
            wait_for_event(&restored, |event| {
                matches!(event, EventMsg::TurnComplete(_))
            }),
        )
        .await?;
        let restored_output = restored_log
            .function_call_output_text(RESTORED_CALL)
            .expect("restored child receives spawn result");
        if !restored_allows {
            assert!(restored_output.contains("Parent-history forks are disabled"));
            assert!(grandchildren.try_recv().is_err());
            return Ok(());
        }
        let spawned: serde_json::Value = serde_json::from_str(&restored_output)?;
        match version {
            Version::V1 => assert!(spawned["agent_id"].as_str().is_some()),
            Version::V2 => {
                assert_eq!(
                    spawned["task_name"],
                    json!("/root/history-worker/grandchild")
                );
            }
        }
        let grandchild_request =
            wait_for_child_request(&grandchild_log, GRANDCHILD_TASK, RESTORED_CALL).await?;
        assert!(grandchild_request.body_contains_text(RESTORED_PROMPT));
        assert!(grandchild_request.body_contains_text(ROLE_INSTRUCTIONS));
    }
    Ok(())
}
