use super::*;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::BaseInstructionsProvenance;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[path = "subagent_role_instruction_failure_tests.rs"]
mod failure_tests;

const ROLE_BASE: &str = "Role-owned literal base instructions.";
const ROLE_DEVELOPER: &str = "Separate role-owned developer instructions.";
const PARENT_BASE: &str = "Inherited custom parent base instructions.";
const SEED_PROMPT: &str = "role-file inherited-history sentinel";
const DELEGATE_PROMPT: &str = "delegate the role-file assignment";
const CHANGE_PROMPT: &str = "change the role-file child's model";
const RESUME_PROMPT: &str = "continue the restored role-file child";

#[derive(Clone, Copy)]
enum Runtime {
    V1,
    V2,
}

#[derive(Clone, Copy)]
enum InstructionPath {
    Relative,
    Absolute,
    Omitted,
}

#[derive(Clone, Copy)]
enum InheritedHistory {
    Fresh,
    Full,
}

fn uses_requested_child_model(request: &wiremock::Request) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .is_some_and(|body| body["model"] == REQUESTED_MODEL)
}

#[test_case(Runtime::V1, InstructionPath::Relative, InheritedHistory::Fresh; "v1 relative fresh")]
#[test_case(Runtime::V1, InstructionPath::Absolute, InheritedHistory::Full; "v1 absolute inherited")]
#[test_case(Runtime::V2, InstructionPath::Absolute, InheritedHistory::Fresh; "v2 absolute fresh")]
#[test_case(Runtime::V2, InstructionPath::Relative, InheritedHistory::Full; "v2 relative inherited")]
#[test_case(Runtime::V1, InstructionPath::Omitted, InheritedHistory::Fresh; "v1 omitted inherits custom")]
#[test_case(Runtime::V2, InstructionPath::Omitted, InheritedHistory::Fresh; "v2 omitted inherits custom")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_instructions_survive_spawn_model_change_and_cold_resume(
    runtime: Runtime,
    instruction_path: InstructionPath,
    inherited_history: InheritedHistory,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let expected_base = match instruction_path {
        InstructionPath::Relative | InstructionPath::Absolute => ROLE_BASE,
        InstructionPath::Omitted => PARENT_BASE,
    };
    let namespace = match runtime {
        Runtime::V1 => MULTI_AGENT_V1_NAMESPACE,
        Runtime::V2 => MULTI_AGENT_V2_NAMESPACE,
    };
    let mut spawn_args = json!({
        "message": CHILD_PROMPT,
        "agent_type": "custom",
        "model": REQUESTED_MODEL,
    });
    match runtime {
        Runtime::V1 => {
            spawn_args["fork_context"] = json!(matches!(inherited_history, InheritedHistory::Full));
        }
        Runtime::V2 => {
            spawn_args["task_name"] = json!("role-worker");
            spawn_args["fork_turns"] = json!(match inherited_history {
                InheritedHistory::Fresh => "none",
                InheritedHistory::Full => "all",
            });
        }
    }
    let _seed = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !uses_requested_child_model(request)
                && body_contains(request, SEED_PROMPT)
                && !body_contains(request, DELEGATE_PROMPT)
        },
        sse(vec![
            ev_response_created("role-seed"),
            ev_assistant_message("role-seed-message", "seed ready"),
            ev_completed("role-seed"),
        ]),
    )
    .await;
    let _spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !uses_requested_child_model(request)
                && body_contains(request, DELEGATE_PROMPT)
                && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("role-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                namespace,
                "spawn_agent",
                &spawn_args.to_string(),
            ),
            ev_completed("role-spawn"),
        ]),
    )
    .await;
    let _parent_done = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !uses_requested_child_model(request) && body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("role-parent-done"),
            ev_assistant_message("role-parent-message", "delegation complete"),
            ev_completed("role-parent-done"),
        ]),
    )
    .await;
    let initial_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            uses_requested_child_model(request)
                && !body_contains(request, CHANGE_PROMPT)
                && !body_contains(request, RESUME_PROMPT)
        },
        sse(vec![
            ev_response_created("role-child"),
            ev_assistant_message("role-child-message", "child ready"),
            ev_completed("role-child"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_history_mode(ThreadHistoryMode::Legacy)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
            match runtime {
                Runtime::V1 => config.features.disable(Feature::MultiAgentV2),
                Runtime::V2 => config.features.enable(Feature::MultiAgentV2),
            }
            .expect("select agent runtime");
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Plaintext;
            config.agent_allow_history_forks = matches!(inherited_history, InheritedHistory::Full);
            config.model = Some(INHERITED_MODEL.to_string());
            config.base_instructions = Some(PARENT_BASE.to_string());
            config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
            let role_dir = config.codex_home.join("roles");
            fs::create_dir_all(&role_dir).expect("create host role directory");
            let base_path = role_dir.join("base.md");
            fs::write(&base_path, format!("  {ROLE_BASE}  \n")).expect("write role base file");
            let role_path = role_dir.join("custom.toml");
            let mut role_config = json!({
                "model": INHERITED_MODEL,
                "developer_instructions": ROLE_DEVELOPER,
            });
            match instruction_path {
                InstructionPath::Relative => {
                    role_config["model_instructions_file"] = json!("base.md");
                }
                InstructionPath::Absolute => {
                    role_config["model_instructions_file"] = json!(base_path.to_string_lossy());
                }
                InstructionPath::Omitted => {}
            }
            let role_toml = toml::to_string(&role_config).expect("serialize role configuration");
            fs::write(&role_path, role_toml).expect("write role configuration");
            config.agent_roles.insert(
                "custom".to_string(),
                AgentRoleConfig {
                    description: Some("Role-file integration fixture".to_string()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn(SEED_PROMPT).await?;
    test.submit_turn(DELEGATE_PROMPT).await?;

    let request = wait_for_request_with_model(&initial_request, REQUESTED_MODEL).await?;
    assert_eq!(
        (
            request.instructions_text(),
            request
                .message_input_texts("developer")
                .contains(&ROLE_DEVELOPER.to_string()),
            request.body_contains_text(SEED_PROMPT),
        ),
        (
            expected_base.to_string(),
            true,
            matches!(inherited_history, InheritedHistory::Full),
        )
    );
    let child_id = ThreadId::from_string(&wait_for_spawned_thread_id(&test).await?)?;
    let child = test.thread_manager.get_thread(child_id).await?;
    assert!(matches!(
        wait_for_terminal_status(&child).await?,
        AgentStatus::Completed(_)
    ));
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let changed_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CHANGE_PROMPT),
        sse(vec![
            ev_response_created("role-model-change"),
            ev_assistant_message("role-model-change-message", "model changed"),
            ev_completed("role-model-change"),
        ]),
    )
    .await;
    child
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: CHANGE_PROMPT.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                model: Some(INHERITED_MODEL.to_string()),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let request = changed_request.single_request();
    assert_eq!(
        (
            request.body_json()["model"].clone(),
            request.instructions_text()
        ),
        (json!(INHERITED_MODEL), expected_base.to_string())
    );

    child.ensure_rollout_materialized().await;
    child.flush_rollout().await?;
    let rollout_path = child.rollout_path().context("expected child rollout")?;
    let history = codex_rollout::RolloutRecorder::get_rollout_history(&rollout_path).await?;
    assert_eq!(
        history.get_base_instructions(),
        Some(BaseInstructions {
            text: expected_base.to_string(),
            provenance: Some(BaseInstructionsProvenance::Custom),
        })
    );
    child.shutdown_durably_and_wait().await?;
    test.thread_manager
        .remove_thread_if_matches(&child_id, &child)
        .await
        .context("remove the stopped exact child instance")?;

    let resumed_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, RESUME_PROMPT),
        sse(vec![
            ev_response_created("role-resumed"),
            ev_assistant_message("role-resumed-message", "resumed"),
            ev_completed("role-resumed"),
        ]),
    )
    .await;
    let mut resume_config = test.config.clone();
    resume_config.base_instructions = None;
    resume_config.base_instructions_provenance = None;
    let resumed = test
        .thread_manager
        .resume_thread_from_rollout(
            resume_config,
            rollout_path,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(resumed.thread_id, child_id);
    assert!(!Arc::ptr_eq(&child, &resumed.thread));
    resumed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: RESUME_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = resumed_request.single_request();
    assert_eq!(
        (
            request.body_json()["model"].clone(),
            request.instructions_text()
        ),
        (json!(INHERITED_MODEL), expected_base.to_string())
    );
    resumed.thread.shutdown_durably_and_wait().await?;
    Ok(())
}
