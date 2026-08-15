use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_fake_parented_rollout_with_explicit_thread_id;
use codex_app_server_protocol::AgentAlias;
use codex_app_server_protocol::AgentAliasListParams;
use codex_app_server_protocol::AgentAliasListResponse;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::AgentControlAction;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentControlParams;
use codex_app_server_protocol::AgentControlResponse;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::AgentObservationBinding;
use codex_app_server_protocol::AgentObservationMode;
use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserAgentControlAction as AuditAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource as ProtocolSessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn agent_alias_list_projects_committed_v1_aliases_in_ref_order() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const CHILD_PROMPT: &str = "child alias task";
    const PARENT_PROMPT: &str = "spawn child for alias list";
    const SPAWN_CALL_ID: &str = "spawn-agent-alias";

    let server = responses::start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "x",
    }))?;
    let _parent_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, PARENT_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-alias-parent"),
            responses::ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                "multi_agent_v1",
                "spawn_agent",
                &spawn_args,
            ),
            responses::ev_completed("resp-agent-alias-parent"),
        ]),
    )
    .await;
    let _child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            responses::body_contains(request, CHILD_PROMPT)
                && !responses::body_contains(request, SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-agent-alias-child"),
            responses::ev_assistant_message("msg-agent-alias-child", "child done"),
            responses::ev_completed("resp-agent-alias-child"),
        ]),
    )
    .await;
    let _parent_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-alias-follow-up"),
            responses::ev_assistant_message("msg-agent-alias-follow-up", "parent done"),
            responses::ev_completed("resp-agent-alias-follow-up"),
        ]),
    )
    .await;
    const DIRECT_PROMPT: &str = "user-authored direct prompt";
    let direct_prompt_turn = responses::mount_sse_once_match_with_delay(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, DIRECT_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-control-prompt"),
            responses::ev_assistant_message("msg-agent-control-prompt", "direct prompt done"),
            responses::ev_completed("resp-agent-control-prompt"),
        ]),
        Duration::from_secs(/*secs*/ 5),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Collab)
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let (child_thread_id, child_nickname) = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if let ThreadItem::CollabAgentToolCall {
                id,
                receiver_thread_ids,
                receiver_agents,
                ..
            } = completed.item
                && id == SPAWN_CALL_ID
                && let Some(child_thread_id) = receiver_thread_ids.first().cloned()
                && let Some(child_nickname) = receiver_agents
                    .first()
                    .and_then(|agent| agent.agent_nickname.clone())
            {
                return Ok::<_, anyhow::Error>((child_thread_id, child_nickname));
            }
        }
    })
    .await??;

    let first_page: AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: AgentAliasListParams {
                root_thread_id: thread.id.clone(),
                cursor: None,
                limit: Some(1),
            },
        })
        .await?;
    assert_eq!(
        first_page,
        AgentAliasListResponse {
            data: vec![AgentAlias {
                thread_id: thread.id.clone(),
                agent_ref: "1".to_string(),
                nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                state: AgentAliasState::Active,
            }],
            next_cursor: Some("1".to_string()),
        }
    );

    let second_page: AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: AgentAliasListParams {
                root_thread_id: thread.id.clone(),
                cursor: first_page.next_cursor,
                limit: Some(1),
            },
        })
        .await?;
    assert_eq!(
        second_page,
        AgentAliasListResponse {
            data: vec![AgentAlias {
                thread_id: child_thread_id.clone(),
                agent_ref: "2".to_string(),
                nickname: Some(child_nickname.clone()),
                state: AgentAliasState::Active,
            }],
            next_cursor: None,
        }
    );
    let child_scoped_page: AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: AgentAliasListParams {
                root_thread_id: child_thread_id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(
        child_scoped_page,
        AgentAliasListResponse {
            data: vec![first_page.data[0].clone(), second_page.data[0].clone()],
            next_cursor: None,
        }
    );

    let response: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: thread.id.clone(),
                authored_selector: Some("ref:2".to_string()),
                action: AgentControlAction::Prompt {
                    target: "2".to_string(),
                    input: vec![UserInput::Text {
                        text: DIRECT_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    assert_eq!(response.audit_warning, None);
    let AgentControlOutcome::Prompted {
        target_thread_id,
        submission_id,
        queued,
        post_admission_warning,
    } = response.outcome
    else {
        panic!("agent prompt should return prompted");
    };
    assert_eq!(target_thread_id, child_thread_id);
    assert!(!submission_id.is_empty());
    assert!(!queued);
    assert_eq!(post_admission_warning, None);
    let prompt_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Prompt,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    let prompt_audit_id = prompt_audit.id().to_string();
    assert_eq!(
        prompt_audit,
        ThreadItem::UserAgentControl {
            id: prompt_audit_id,
            action: AuditAgentControlAction::Prompt,
            authored_selector: Some("ref:2".to_string()),
            target_thread_id: Some(child_thread_id.clone()),
            previous_owner_session_id: None,
            new_owner_session_id: None,
            agent_ref: Some("2".to_string()),
            nickname: Some(child_nickname.clone()),
            role: None,
            prompt_preview: Some(DIRECT_PROMPT.to_string()),
            resumed_target: false,
            fork_mode: None,
            observe_commentary: Some(false),
            final_response: Some(AgentFinalResponseHandling::Wake),
            target_messages: Some(false),
            queue_input: Some(false),
            status: UserAgentControlStatus::Succeeded,
            error: None,
        }
    );

    let observed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Observe {
                    target: "2".to_string(),
                    response_handling: AgentObservationMode::Presentation,
                },
            },
        })
        .await?;
    assert_eq!(observed.audit_warning, None);
    assert_eq!(
        observed.outcome,
        AgentControlOutcome::Observed {
            target_thread_id: child_thread_id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Wake,
            response_handling: AgentFinalResponseHandling::Presentation,
            binding: codex_app_server_protocol::AgentObservationBinding::ActiveTurn,
        }
    );
    let observe_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Observe,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    let observe_audit_id = observe_audit.id().to_string();
    assert_eq!(
        observe_audit,
        ThreadItem::UserAgentControl {
            id: observe_audit_id,
            action: AuditAgentControlAction::Observe,
            authored_selector: Some("2".to_string()),
            target_thread_id: Some(child_thread_id.clone()),
            previous_owner_session_id: None,
            new_owner_session_id: None,
            agent_ref: Some("2".to_string()),
            nickname: Some(child_nickname.clone()),
            role: None,
            prompt_preview: None,
            resumed_target: false,
            fork_mode: None,
            observe_commentary: Some(false),
            final_response: Some(AgentFinalResponseHandling::Presentation),
            target_messages: None,
            queue_input: None,
            status: UserAgentControlStatus::Succeeded,
            error: None,
        }
    );
    let request_id = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread.id.clone(),
            include_turns: true,
        })
        .await?;
    let source_history: ThreadReadResponse =
        timeout(DEFAULT_READ_TIMEOUT, app.read_response(request_id)).await??;
    let persisted_controls = source_history
        .thread
        .turns
        .into_iter()
        .flat_map(|turn| turn.items)
        .filter(|item| matches!(item, ThreadItem::UserAgentControl { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_controls,
        vec![prompt_audit.clone(), observe_audit.clone()]
    );
    direct_prompt_turn.single_request();

    const USER_SPAWN_PROMPT: &str = "user-authored child spawn";
    let user_spawn_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, USER_SPAWN_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-control-spawn"),
            responses::ev_assistant_message("msg-agent-control-spawn", "spawned child done"),
            responses::ev_completed("resp-agent-control-spawn"),
        ]),
    )
    .await;
    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: USER_SPAWN_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert_eq!(spawned.audit_warning, None);
    let AgentControlOutcome::Spawned {
        target_thread_id: spawned_thread_id,
        agent_ref,
        nickname,
        post_admission_warning,
    } = spawned.outcome
    else {
        panic!("agent spawn should return spawned");
    };
    assert_ne!(spawned_thread_id, child_thread_id);
    assert_eq!(agent_ref.as_deref(), Some("3"));
    assert!(nickname.is_some());
    assert_eq!(post_admission_warning, None);
    user_spawn_turn.single_request();

    const CLOSE_PROMPT: &str = "close the child through its durable ref";
    const CLOSE_CALL_ID: &str = "close-agent-alias";
    let close_args = serde_json::to_string(&json!({ "target": "2" }))?;
    let _close_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, CLOSE_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-alias-close"),
            responses::ev_function_call_with_namespace(
                CLOSE_CALL_ID,
                "multi_agent_v1",
                "close_agent",
                &close_args,
            ),
            responses::ev_completed("resp-agent-alias-close"),
        ]),
    )
    .await;
    let _close_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, CLOSE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-agent-alias-close-follow-up"),
            responses::ev_assistant_message("msg-agent-alias-close-follow-up", "child closed"),
            responses::ev_completed("resp-agent-alias-close-follow-up"),
        ]),
    )
    .await;
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: CLOSE_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::CollabAgentToolCall { ref id, .. } if id == CLOSE_CALL_ID
            ) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;

    let resumed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Resume {
                    target: "2".to_string(),
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    assert_eq!(resumed.audit_warning, None);
    assert_eq!(
        resumed.outcome,
        AgentControlOutcome::Resumed {
            target_thread_id: child_thread_id.clone(),
            agent_ref: Some("2".to_string()),
            nickname: Some(child_nickname.clone()),
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
        }
    );

    let foreign = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let request_id = app
        .send_raw_request(
            "agent/control",
            Some(serde_json::to_value(AgentControlParams {
                source_thread_id: thread.id.clone(),
                authored_selector: Some(foreign.thread.id.clone()),
                action: AgentControlAction::Prompt {
                    target: foreign.thread.id.clone(),
                    input: vec![UserInput::Text {
                        text: "must not cross roots".to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: None,
                },
            })?),
        )
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(request_id))
        .await?;
    assert!(
        error
            .error
            .message
            .contains("is not controlled by this root"),
        "unexpected foreign-target error: {error:?}"
    );
    let rejected_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Prompt,
                    status: UserAgentControlStatus::Failed,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    let rejected_audit_id = rejected_audit.id().to_string();
    assert_eq!(
        rejected_audit,
        ThreadItem::UserAgentControl {
            id: rejected_audit_id,
            action: AuditAgentControlAction::Prompt,
            authored_selector: Some(foreign.thread.id.clone()),
            target_thread_id: Some(foreign.thread.id.clone()),
            previous_owner_session_id: None,
            new_owner_session_id: None,
            agent_ref: None,
            nickname: None,
            role: None,
            prompt_preview: Some("must not cross roots".to_string()),
            resumed_target: false,
            fork_mode: None,
            observe_commentary: Some(false),
            final_response: Some(AgentFinalResponseHandling::Passive),
            target_messages: Some(false),
            queue_input: Some(false),
            status: UserAgentControlStatus::Failed,
            error: Some(format!(
                "agent {} is not controlled by this root; use resume_agent to adopt it",
                foreign.thread.id
            )),
        }
    );
    Ok(())
}

#[tokio::test]
async fn unaliased_legacy_child_alias_list_uses_persisted_root() -> Result<()> {
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new("http://127.0.0.1:1")
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;
    let sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let state_db = StateRuntime::init(sqlite.clone(), "mock_provider".to_string()).await?;
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: codex_home.path().to_path_buf(),
            sqlite,
            default_model_provider_id: "mock_provider".to_string(),
        },
        /*state_db*/ Some(state_db.clone()),
    );
    let root_thread_id =
        ThreadId::from_u128(/*value*/ 0x018f_0000_0000_7000_8000_0000_0000_0041);
    let child_thread_id =
        ThreadId::from_u128(/*value*/ 0x018f_0000_0000_7000_8000_0000_0000_0042);
    store
        .create_thread(CreateThreadParams {
            session_id: root_thread_id.into(),
            thread_id: root_thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: ProtocolSessionSource::Cli,
            thread_source: None,
            originator: "legacy-root-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            history_mode: Default::default(),
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(codex_home.path().to_path_buf()),
                model_provider: "mock_provider".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        })
        .await?;
    store
        .create_thread(CreateThreadParams {
            // Older V1 children stored their own thread UUID here rather than the owning root.
            session_id: child_thread_id.into(),
            thread_id: child_thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: Some(root_thread_id),
            source: ProtocolSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("legacy-worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            thread_source: None,
            originator: "legacy-child-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            history_mode: Default::default(),
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(codex_home.path().to_path_buf()),
                model_provider: "mock_provider".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        })
        .await?;
    create_fake_parented_rollout_with_explicit_thread_id(
        codex_home.path(),
        "2025-01-05T10-00-00",
        "2025-01-05T10:00:00Z",
        "Saved root message",
        Some("mock_provider"),
        /*git_info*/ None,
        ProtocolSessionSource::Cli,
        root_thread_id,
        root_thread_id.into(),
        /*parent_thread_id*/ None,
    )?;
    create_fake_parented_rollout_with_explicit_thread_id(
        codex_home.path(),
        "2025-01-05T11-00-00",
        "2025-01-05T11:00:00Z",
        "Saved child message",
        Some("mock_provider"),
        /*git_info*/ None,
        ProtocolSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: Some("legacy-worker".to_string()),
            agent_role: Some("worker".to_string()),
        }),
        child_thread_id,
        child_thread_id.into(),
        /*parent_thread_id*/ Some(root_thread_id),
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let aliases: AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: AgentAliasListParams {
                root_thread_id: child_thread_id.to_string(),
                cursor: None,
                limit: None,
            },
        })
        .await?;

    assert_eq!(
        aliases,
        AgentAliasListResponse {
            data: vec![
                AgentAlias {
                    thread_id: root_thread_id.to_string(),
                    agent_ref: "1".to_string(),
                    nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                    state: AgentAliasState::Active,
                },
                AgentAlias {
                    thread_id: child_thread_id.to_string(),
                    agent_ref: "2".to_string(),
                    nickname: Some("legacy-worker".to_string()),
                    state: AgentAliasState::Active,
                },
            ],
            next_cursor: None,
        }
    );
    assert_eq!(
        state_db
            .find_current_agent_alias_by_thread(child_thread_id)
            .await?,
        Some(codex_state::AgentAliasRecord {
            session_id: root_thread_id.into(),
            thread_id: child_thread_id,
            agent_ref: 2,
            nickname: Some("legacy-worker".to_string()),
            state: codex_state::AgentAliasState::Active,
        })
    );
    Ok(())
}
