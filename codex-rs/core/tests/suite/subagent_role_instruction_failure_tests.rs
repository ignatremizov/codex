use super::*;

#[derive(Clone, Copy)]
enum RejectedRoleSpawn {
    MissingFile,
    EmptyFile,
    BlankFile,
    InvalidText,
    Directory,
    HistoryDisabled,
}

#[test_case(Runtime::V1, RejectedRoleSpawn::MissingFile; "v1 missing file")]
#[test_case(Runtime::V2, RejectedRoleSpawn::MissingFile; "v2 missing file")]
#[test_case(Runtime::V1, RejectedRoleSpawn::EmptyFile; "v1 empty file")]
#[test_case(Runtime::V2, RejectedRoleSpawn::EmptyFile; "v2 empty file")]
#[test_case(Runtime::V1, RejectedRoleSpawn::BlankFile; "v1 blank file")]
#[test_case(Runtime::V2, RejectedRoleSpawn::BlankFile; "v2 blank file")]
#[test_case(Runtime::V1, RejectedRoleSpawn::InvalidText; "v1 invalid text")]
#[test_case(Runtime::V2, RejectedRoleSpawn::InvalidText; "v2 invalid text")]
#[test_case(Runtime::V1, RejectedRoleSpawn::Directory; "v1 unreadable file")]
#[test_case(Runtime::V2, RejectedRoleSpawn::Directory; "v2 unreadable file")]
#[test_case(Runtime::V1, RejectedRoleSpawn::HistoryDisabled; "v1 role file cannot bypass history gate")]
#[test_case(Runtime::V2, RejectedRoleSpawn::HistoryDisabled; "v2 role file cannot bypass history gate")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_role_spawn_returns_actionable_error_without_creating_child(
    runtime: Runtime,
    rejection: RejectedRoleSpawn,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    const PARENT_DEVELOPER: &str = "Unchanged parent developer instructions.";
    let server = start_mock_server().await;
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
            spawn_args["fork_context"] =
                json!(matches!(rejection, RejectedRoleSpawn::HistoryDisabled));
        }
        Runtime::V2 => {
            spawn_args["task_name"] = json!("rejected-role-worker");
            spawn_args["fork_turns"] = json!(match rejection {
                RejectedRoleSpawn::HistoryDisabled => "all",
                RejectedRoleSpawn::MissingFile
                | RejectedRoleSpawn::EmptyFile
                | RejectedRoleSpawn::BlankFile
                | RejectedRoleSpawn::InvalidText
                | RejectedRoleSpawn::Directory => "none",
            });
        }
    }
    let _spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| !body_contains(request, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("rejected-role-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                namespace,
                "spawn_agent",
                &spawn_args.to_string(),
            ),
            ev_completed("rejected-role-spawn"),
        ]),
    )
    .await;
    let parent_continuation = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("rejected-role-parent"),
            ev_assistant_message("rejected-role-parent-message", "spawn was rejected"),
            ev_completed("rejected-role-parent"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
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
        config.agent_allow_history_forks = false;
        config.model = Some(INHERITED_MODEL.to_string());
        config.base_instructions = Some(PARENT_BASE.to_string());
        config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
        config.developer_instructions = Some(PARENT_DEVELOPER.to_string());
        let role_dir = config.codex_home.join("roles");
        fs::create_dir_all(&role_dir).expect("create host role directory");
        let base_path = role_dir.join("base.md");
        match rejection {
            RejectedRoleSpawn::MissingFile => {}
            RejectedRoleSpawn::EmptyFile => {
                fs::write(&base_path, "").expect("write empty file");
            }
            RejectedRoleSpawn::BlankFile => {
                fs::write(&base_path, " \n\t ").expect("write blank file");
            }
            RejectedRoleSpawn::InvalidText => {
                fs::write(&base_path, b"\xff").expect("write invalid text");
            }
            RejectedRoleSpawn::Directory => {
                fs::create_dir(&base_path).expect("create unreadable file target");
            }
            RejectedRoleSpawn::HistoryDisabled => {
                fs::write(&base_path, ROLE_BASE).expect("write valid role file");
            }
        }
        let role_path = role_dir.join("custom.toml");
        let role_toml = toml::to_string(&json!({
            "model": REQUESTED_MODEL,
            "model_instructions_file": "base.md",
            "developer_instructions": ROLE_DEVELOPER,
        }))
        .expect("serialize role configuration");
        fs::write(&role_path, role_toml).expect("write role configuration");
        config.agent_roles.insert(
            "custom".to_string(),
            AgentRoleConfig {
                description: Some("Rejected role-file fixture".to_string()),
                config_file: Some(role_path.to_path_buf()),
                nickname_candidates: None,
            },
        );
    });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn(DELEGATE_PROMPT).await?;

    let request = parent_continuation.single_request();
    let error = request
        .function_call_output_text(SPAWN_CALL_ID)
        .context("spawn error returned to the parent model")?;
    match rejection {
        RejectedRoleSpawn::HistoryDisabled => {
            assert!(
                error.contains("Parent-history forks are disabled"),
                "{error}"
            );
        }
        RejectedRoleSpawn::MissingFile
        | RejectedRoleSpawn::EmptyFile
        | RejectedRoleSpawn::BlankFile
        | RejectedRoleSpawn::InvalidText
        | RejectedRoleSpawn::Directory => {
            let resolved_path = test.config.codex_home.join("roles/base.md");
            assert!(error.contains("agent role `custom`"), "{error}");
            assert!(error.contains("model instructions file"), "{error}");
            assert!(
                error.contains(resolved_path.to_string_lossy().as_ref()),
                "{error}"
            );
        }
    }
    assert_eq!(
        (
            request.body_json()["model"].clone(),
            request.instructions_text(),
            request
                .message_input_texts("developer")
                .contains(&PARENT_DEVELOPER.to_string()),
            request
                .message_input_texts("developer")
                .contains(&ROLE_DEVELOPER.to_string()),
            test.thread_manager.list_thread_ids().await,
        ),
        (
            json!(INHERITED_MODEL),
            PARENT_BASE.to_string(),
            true,
            false,
            vec![test.session_configured.thread_id],
        )
    );
    Ok(())
}
