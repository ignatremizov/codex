use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_models_cache;
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
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserAgentControlAction as AuditAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::InMemoryThreadStore;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

use crate::common::DEFAULT_READ_TIMEOUT;

fn agent_control_outcome(response: AgentControlResponse) -> AgentControlOutcome {
    assert_eq!(response.audit_warning, None);
    response.outcome
}

async fn wait_for_thread_turn_completed(app: &mut TestAppServer, thread_id: &str) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == thread_id {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await?
}

#[tokio::test]
async fn user_control_audit_does_not_finish_an_active_source_turn() -> Result<()> {
    const SOURCE_PROMPT: &str = "keep the source turn active during user control";

    let server = responses::start_mock_server().await;
    let source_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(SOURCE_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-active-user-control-source"),
            responses::ev_assistant_message(
                "msg-active-user-control-source",
                "source turn completed",
            ),
            responses::ev_completed("resp-active-user-control-source"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;
    let started: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: root.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: SOURCE_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while source_turn.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    let close_result: Result<AgentControlResponse, _> = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("1".to_string()),
                action: AgentControlAction::Close {
                    target: "1".to_string(),
                },
            },
        })
        .await;
    let error = close_result.expect_err("Main cannot close itself");
    assert!(error.to_string().contains("cannot close itself"));

    let audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Close,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed);
            }
        }
    })
    .await??;
    assert_eq!(audit.thread_id, root.thread.id);
    assert_eq!(audit.turn_id, started.turn.id);
    assert!(matches!(
        app.send_thread_read_request(ThreadReadParams {
            thread_id: root.thread.id.clone(),
            include_turns: false,
        })
        .await?
        .thread
        .status,
        ThreadStatus::Active { .. }
    ));

    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == root.thread.id {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    source_turn.expect(1).await;
    Ok(())
}

#[tokio::test]
async fn child_can_prompt_and_observe_main_but_cannot_close_it_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        child_can_prompt_and_observe_main_but_cannot_close_it(multi_agent_v2).await?;
    }
    Ok(())
}

async fn child_can_prompt_and_observe_main_but_cannot_close_it(multi_agent_v2: bool) -> Result<()> {
    const MAIN_PROMPT: &str = "reply to the user-authored child-to-Main prompt";

    let server = responses::start_mock_server().await;
    let main_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(MAIN_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-child-to-main"),
            responses::ev_assistant_message("msg-child-to-main", "Main received the prompt"),
            responses::ev_completed("resp-child-to-main"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;
    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let main = app.start_thread(ThreadStartParams::default()).await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: main.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: child_thread_id,
        ..
    } = agent_control_outcome(spawned)
    else {
        panic!("idle child should spawn");
    };

    let prompted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: child_thread_id.clone(),
                authored_selector: Some("mAiN".to_string()),
                action: AgentControlAction::Prompt {
                    target: "mAiN".to_string(),
                    input: vec![UserInput::Text {
                        text: MAIN_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(prompted),
        AgentControlOutcome::Prompted {
            ref target_thread_id,
            ..
        } if target_thread_id == &main.thread.id
    ));
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
    assert!(matches!(
        prompt_audit,
        ThreadItem::UserAgentControl {
            authored_selector: Some(ref selector),
            target_thread_id: Some(ref target_thread_id),
            agent_ref: Some(ref agent_ref),
            nickname: Some(ref nickname),
            status: UserAgentControlStatus::Succeeded,
            ..
        } if selector == "mAiN"
            && target_thread_id == &main.thread.id
            && agent_ref == "1"
            && nickname.as_str() == codex_protocol::MAIN_AGENT_NICKNAME
    ));

    let observed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: child_thread_id.clone(),
                authored_selector: Some("MAIN".to_string()),
                action: AgentControlAction::Observe {
                    target: "MAIN".to_string(),
                    response_handling: AgentObservationMode::Presentation,
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(observed),
        AgentControlOutcome::Observed {
            target_thread_id: main.thread.id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Wake,
            response_handling: AgentFinalResponseHandling::Presentation,
            binding: AgentObservationBinding::ActiveTurn,
        }
    );

    let close_result: Result<AgentControlResponse, _> = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: child_thread_id,
                authored_selector: Some("main".to_string()),
                action: AgentControlAction::Close {
                    target: "main".to_string(),
                },
            },
        })
        .await;
    let error = close_result.expect_err("a child cannot close Main");
    assert!(error.to_string().contains("cannot close Main"));
    main_turn.expect(1).await;
    Ok(())
}

#[tokio::test]
async fn user_control_reserved_prompt_consumes_v1_spawn_reservation() -> Result<()> {
    const PROMPT: &str = "first prompt authored inside the idle V1 child";
    const ROOT_CHECK_PROMPT: &str = "check the V1 source context";

    let server = responses::start_mock_server().await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-v1-first-prompt"),
            responses::ev_assistant_message(
                "msg-user-control-v1-first-prompt",
                "V1 first prompt done",
            ),
            responses::ev_completed("resp-user-control-v1-first-prompt"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;
    let root_check_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(ROOT_CHECK_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-v1-root-check"),
            responses::ev_assistant_message(
                "msg-user-control-v1-root-check",
                "V1 source context checked",
            ),
            responses::ev_completed("resp-user-control-v1-root-check"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id,
        agent_ref,
        nickname,
        ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an idle V1 child");
    };
    let agent_ref = agent_ref.expect("V1 child should have a durable agent ref");
    let nickname = nickname.expect("V1 child should have a durable nickname");

    let prompted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::ReservedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(prompted),
        AgentControlOutcome::ReservedPrompted {
            target_thread_id: ref prompted_thread_id,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    let observed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::Observe {
                    target: target_thread_id.clone(),
                    response_handling: AgentObservationMode::Passive,
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(observed),
        AgentControlOutcome::Observed {
            target_thread_id: target_thread_id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Wake,
            response_handling: AgentFinalResponseHandling::Passive,
            binding: AgentObservationBinding::ActiveTurn,
        }
    );

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
    assert!(matches!(
        prompt_audit,
        ThreadItem::UserAgentControl {
            action: AuditAgentControlAction::Prompt,
            authored_selector: None,
            target_thread_id: Some(ref audited_thread_id),
            prompt_preview: Some(ref prompt_preview),
            observe_commentary: Some(false),
            final_response: Some(AgentFinalResponseHandling::Wake),
            target_messages: Some(false),
            queue_input: Some(false),
            status: UserAgentControlStatus::Succeeded,
            ..
        } if audited_thread_id == &target_thread_id && prompt_preview == PROMPT
    ));

    child_turn.expect(1).await;
    let request = child_turn.single_request();
    assert!(request.body_contains_text(PROMPT));
    assert!(request.inputs_of_type("agent_message").is_empty());

    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: root.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: ROOT_CHECK_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    root_check_turn.expect(1).await;
    let root_request = root_check_turn.single_request();
    let task_context = root_request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.contains("<user_agent_task>"))
        .expect("V1 source context should include the user-agent task");
    assert!(task_context.contains(&format!(r#""agent_id":"{target_thread_id}""#)));
    assert!(task_context.contains(&format!(r#""ref":"{agent_ref}""#)));
    assert!(task_context.contains(&format!(r#""nickname":"{nickname}""#)));
    assert!(!task_context.contains("\"agent_path\""));
    assert!(root_request.body_contains_text(PROMPT));
    Ok(())
}

#[tokio::test]
async fn user_spawn_reports_post_admission_observation_failure_without_discarding_child()
-> Result<()> {
    const PROMPT: &str = "preserve the user-authored child after prompt admission";

    let server = responses::start_mock_server().await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-spawn-post-admission-failure"),
            responses::ev_assistant_message(
                "msg-user-spawn-post-admission-failure",
                "preserved child done",
            ),
            responses::ev_completed("resp-user-spawn-post-admission-failure"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;

    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .with_root_config(&format!(
            r#"experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}"#
        ))
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;
    store
        .fail_agent_response_observation_flushes_after(
            /*successful_flushes*/ 1, /*failed_flushes*/ 1,
        )
        .await;

    let response: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("+default".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id,
        agent_ref,
        nickname,
        post_admission_warning,
    } = agent_control_outcome(response)
    else {
        panic!("user control should return its committed spawn");
    };
    let warning =
        post_admission_warning.context("post-admission persistence failure should be reported")?;
    assert!(warning.contains("failed to persist spawned response observation state"));
    assert!(warning.contains("child input was already admitted"));

    let aliases: AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: AgentAliasListParams {
                root_thread_id: root.thread.id.clone(),
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
                    thread_id: root.thread.id.clone(),
                    agent_ref: "1".to_string(),
                    nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                    state: AgentAliasState::Active,
                },
                AgentAlias {
                    thread_id: target_thread_id.clone(),
                    agent_ref: agent_ref.context("spawned child should have a durable ref")?,
                    nickname,
                    state: AgentAliasState::Active,
                },
            ],
            next_cursor: None,
        }
    );

    let audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Spawn,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    assert!(matches!(
        audit,
        ThreadItem::UserAgentControl {
            target_thread_id: Some(ref audited_thread_id),
            status: UserAgentControlStatus::Succeeded,
            error: Some(ref audit_warning),
            ..
        } if audited_thread_id == &target_thread_id && audit_warning == &warning
    ));

    child_turn.expect(1).await;
    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test]
async fn commentary_presentation_keeps_user_task_context_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        commentary_presentation_keeps_user_task_context(multi_agent_v2).await?;
    }
    Ok(())
}

async fn commentary_presentation_keeps_user_task_context(multi_agent_v2: bool) -> Result<()> {
    const SPAWN_PROMPT: &str = "commentary task supplied while spawning";
    const SEED_PROMPT: &str = "seed the direct-prompt target";
    const DIRECT_PROMPT: &str = "commentary task supplied by direct prompt";
    const FIRST_PROMPT: &str = "commentary task supplied inside idle child";
    const ROOT_CHECK_PROMPT: &str = "inspect commentary task links";

    let server = responses::start_mock_server().await;
    let spawn_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(SPAWN_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-commentary-task-spawn"),
            responses::ev_assistant_message(
                "msg-commentary-task-spawn",
                "spawned commentary task done",
            ),
            responses::ev_completed("resp-commentary-task-spawn"),
        ]),
    )
    .await;
    let seed_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(SEED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-commentary-task-seed"),
            responses::ev_assistant_message(
                "msg-commentary-task-seed",
                "direct-prompt target seeded",
            ),
            responses::ev_completed("resp-commentary-task-seed"),
        ]),
    )
    .await;
    let direct_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(DIRECT_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-commentary-task-direct"),
            responses::ev_assistant_message(
                "msg-commentary-task-direct",
                "direct commentary task done",
            ),
            responses::ev_completed("resp-commentary-task-direct"),
        ]),
    )
    .await;
    let first_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(FIRST_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-commentary-task-first"),
            responses::ev_assistant_message(
                "msg-commentary-task-first",
                "first commentary task done",
            ),
            responses::ev_completed("resp-commentary-task-first"),
        ]),
    )
    .await;
    let root_check_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(ROOT_CHECK_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-commentary-task-root-check"),
            responses::ev_assistant_message(
                "msg-commentary-task-root-check",
                "commentary task links checked",
            ),
            responses::ev_completed("resp-commentary-task-root-check"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: SPAWN_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::CommentaryPresentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: spawned_target,
        ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn the prompted target");
    };
    wait_for_thread_turn_completed(&mut app, spawned_target.as_str()).await?;

    let seeded: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: SEED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: direct_target,
        ..
    } = agent_control_outcome(seeded)
    else {
        panic!("user control should spawn the direct-prompt target");
    };
    wait_for_thread_turn_completed(&mut app, direct_target.as_str()).await?;

    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(direct_target.clone()),
                action: AgentControlAction::Prompt {
                    target: direct_target.clone(),
                    input: vec![UserInput::Text {
                        text: DIRECT_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::CommentaryPresentation),
                },
            },
        })
        .await?;
    wait_for_thread_turn_completed(&mut app, direct_target.as_str()).await?;

    let idle: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::CommentaryPresentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: idle_target,
        ..
    } = agent_control_outcome(idle)
    else {
        panic!("user control should spawn the idle first-prompt target");
    };
    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::ReservedPrompt {
                    target: idle_target.clone(),
                    input: vec![UserInput::Text {
                        text: FIRST_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                },
            },
        })
        .await?;
    wait_for_thread_turn_completed(&mut app, idle_target.as_str()).await?;

    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: root.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: ROOT_CHECK_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    root_check_turn.expect(1).await;
    let root_request = root_check_turn.single_request();
    assert!(root_request.body_contains_text("<user_agent_task>"));
    for prompt in [SPAWN_PROMPT, DIRECT_PROMPT, FIRST_PROMPT] {
        assert!(root_request.body_contains_text(prompt));
    }
    spawn_turn.expect(1).await;
    seed_turn.expect(1).await;
    direct_turn.expect(1).await;
    first_turn.expect(1).await;
    Ok(())
}

#[tokio::test]
async fn queued_prompt_waits_for_idle_target_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        queued_prompt_waits_for_idle_target(multi_agent_v2).await?;
    }
    Ok(())
}

#[tokio::test]
async fn queued_prompt_binds_idle_v1_wake_before_target_turn_started_persists() -> Result<()> {
    const QUEUED_PROMPT: &str = "run the queued task with the reserved wake";
    const CHILD_RESULT: &str = "queued task completed under the reserved wake";

    let server = responses::start_mock_server().await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, QUEUED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-queued-reserved-wake-child"),
            responses::ev_assistant_message("msg-queued-reserved-wake-child", CHILD_RESULT),
            responses::ev_completed("resp-queued-reserved-wake-child"),
        ]),
    )
    .await;
    let root_wake = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, CHILD_RESULT),
        responses::sse(vec![
            responses::ev_response_created("resp-queued-reserved-wake-root"),
            responses::ev_assistant_message(
                "msg-queued-reserved-wake-root",
                "reserved wake received",
            ),
            responses::ev_completed("resp-queued-reserved-wake-root"),
        ]),
    )
    .await;

    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .with_root_config(&format!(
            r#"experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}"#
        ))
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an idle V1 child");
    };

    store
        .fail_next_operation(InMemoryThreadStoreFailure::TurnStartedAppend)
        .await;
    let queued: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::QueuedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: QUEUED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(queued),
        AgentControlOutcome::Prompted {
            target_thread_id: ref prompted_thread_id,
            queued: true,
            post_admission_warning: None,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let started: TurnStartedNotification = app.read_notification("turn/started").await?;
            if started.thread_id == target_thread_id && started.agent_queue.is_some() {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while child_turn.requests().is_empty() || root_wake.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .context("queued child completion did not wake the source")?;
    child_turn.single_request();
    root_wake.single_request();

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test]
async fn queued_prompt_degradation_preserves_idle_v1_wake_and_reply_route() -> Result<()> {
    for (failure, label) in [
        (
            InMemoryThreadStoreFailure::AgentResponseObservationFlush,
            "observation",
        ),
        (
            InMemoryThreadStoreFailure::UserAgentTaskContextFlush,
            "task-context",
        ),
    ] {
        queued_prompt_degradation_preserves_idle_v1_reservation(failure, label).await?;
    }
    Ok(())
}

async fn queued_prompt_degradation_preserves_idle_v1_reservation(
    failure: InMemoryThreadStoreFailure,
    label: &str,
) -> Result<()> {
    let queued_prompt = format!("run queued task after {label} degradation");
    let child_result = format!("queued task completed after {label} degradation");
    let child_prompt_match = queued_prompt.clone();
    let root_result_match = child_result.clone();
    let server = responses::start_mock_server().await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| responses::body_contains(request, &child_prompt_match),
        responses::sse(vec![
            responses::ev_response_created("resp-queued-degraded-reservation-child"),
            responses::ev_assistant_message("msg-queued-degraded-reservation-child", &child_result),
            responses::ev_completed("resp-queued-degraded-reservation-child"),
        ]),
    )
    .await;
    let root_wake = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| responses::body_contains(request, &root_result_match),
        responses::sse(vec![
            responses::ev_response_created("resp-queued-degraded-reservation-root"),
            responses::ev_assistant_message(
                "msg-queued-degraded-reservation-root",
                "retained wake received",
            ),
            responses::ev_completed("resp-queued-degraded-reservation-root"),
        ]),
    )
    .await;

    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .with_root_config(&format!(
            r#"experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}"#
        ))
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::new(
                        /*commentary*/ false,
                        AgentFinalResponseHandling::Wake,
                        /*target_messages*/ true,
                        /*queue_input*/ false,
                    )),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an idle V1 child");
    };

    store.fail_next_operation(failure).await;
    let queued: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::QueuedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: queued_prompt,
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(queued),
        AgentControlOutcome::Prompted {
            target_thread_id: ref prompted_thread_id,
            queued: true,
            post_admission_warning: None,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    let started = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let started: TurnStartedNotification = app.read_notification("turn/started").await?;
            if started.thread_id == target_thread_id && started.agent_queue.is_some() {
                return Ok::<_, anyhow::Error>(started);
            }
        }
    })
    .await??;
    assert_eq!(
        started
            .agent_queue
            .expect("queued turn provenance")
            .response_handling,
        Some(AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::Wake,
            /*target_messages*/ false,
            /*queue_input*/ true,
        )),
        "the failed queue-entry policy should degrade without hiding queue provenance"
    );
    timeout(DEFAULT_READ_TIMEOUT, async {
        while child_turn.requests().is_empty() || root_wake.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .context("retained next-turn policy did not wake the source")?;
    child_turn.single_request();
    root_wake.single_request();
    assert!(
        child_turn
            .single_request()
            .body_contains_text("<agent_reply_route>"),
        "the retained m policy should remain in the admitted target input"
    );

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

async fn queued_prompt_waits_for_idle_target(multi_agent_v2: bool) -> Result<()> {
    const ACTIVE_PROMPT: &str = "keep this agent active";
    const QUEUED_PROMPT: &str = "run only after the active turn";

    let server = responses::start_mock_server().await;
    let active_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(ACTIVE_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-queue-active"),
            responses::ev_assistant_message(
                "msg-user-control-queue-active",
                "active turn finished",
            ),
            responses::ev_completed("resp-user-control-queue-active"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;
    let queued_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(QUEUED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-queue-follow-up"),
            responses::ev_assistant_message(
                "msg-user-control-queue-follow-up",
                "queued turn finished",
            ),
            responses::ev_completed("resp-user-control-queue-follow-up"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: ACTIVE_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an active child");
    };
    timeout(DEFAULT_READ_TIMEOUT, async {
        while active_turn.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .context("active child turn did not reach the Responses API")?;

    let deferred: Result<AgentControlResponse, _> = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::QueuedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: QUEUED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await;
    let error = deferred.expect_err("queued prompt must not steer an active target");
    assert!(
        error
            .to_string()
            .contains("queued input requires an idle target"),
        "unexpected queue deferral error: {error:#}"
    );

    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == target_thread_id {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;

    let admitted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::QueuedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: QUEUED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(admitted),
        AgentControlOutcome::Prompted {
            target_thread_id: ref prompted_thread_id,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    active_turn.expect(1).await;
    assert!(
        !active_turn
            .single_request()
            .body_contains_text(QUEUED_PROMPT)
    );
    queued_turn.expect(1).await;
    assert!(
        queued_turn
            .single_request()
            .body_contains_text(QUEUED_PROMPT)
    );

    let history: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: root.thread.id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(
        history
            .thread
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter(|item| matches!(
                item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::QueuedPrompt,
                    ..
                }
            ))
            .count(),
        1,
        "the deferred attempt must not create a failed audit item"
    );
    Ok(())
}

#[tokio::test]
async fn user_control_prompt_reopens_closed_target_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        for admission in [
            ClosedTargetPromptAdmission::Direct,
            ClosedTargetPromptAdmission::Queued,
        ] {
            user_control_prompt_reopens_closed_target(multi_agent_v2, admission).await?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ClosedTargetPromptAdmission {
    Direct,
    Queued,
}

async fn user_control_prompt_reopens_closed_target(
    multi_agent_v2: bool,
    admission: ClosedTargetPromptAdmission,
) -> Result<()> {
    let (prompt, action, audit_action) = match admission {
        ClosedTargetPromptAdmission::Direct => (
            "continue work after the user reopens this agent",
            AgentControlAction::Prompt {
                target: "2".to_string(),
                input: vec![UserInput::Text {
                    text: "continue work after the user reopens this agent".to_string(),
                    text_elements: Vec::new(),
                }],
                response_handling: Some(AgentResponseHandling::new(
                    /*commentary*/ false,
                    AgentFinalResponseHandling::Presentation,
                    /*target_messages*/ false,
                    /*queue_input*/ false,
                )),
            },
            AuditAgentControlAction::Prompt,
        ),
        ClosedTargetPromptAdmission::Queued => (
            "run queued work after the user reopens this agent",
            AgentControlAction::QueuedPrompt {
                target: "2".to_string(),
                input: vec![UserInput::Text {
                    text: "run queued work after the user reopens this agent".to_string(),
                    text_elements: Vec::new(),
                }],
                response_handling: Some(AgentResponseHandling::Presentation),
            },
            AuditAgentControlAction::QueuedPrompt,
        ),
    };

    let server = responses::start_mock_server().await;
    let resumed_turn = responses::mount_sse_once_match(
        &server,
        move |request| request.body_contains_text(prompt),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-resumed-prompt"),
            responses::ev_assistant_message(
                "msg-user-control-resumed-prompt",
                "resumed prompt finished",
            ),
            responses::ev_completed("resp-user-control-resumed-prompt"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an idle child");
    };

    let closed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Close {
                    target: "2".to_string(),
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(closed),
        AgentControlOutcome::Closed {
            target_thread_id: target_thread_id.clone(),
        }
    );

    let prompted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action,
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(prompted),
        AgentControlOutcome::Prompted {
            target_thread_id: ref prompted_thread_id,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    let prompt_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action,
                    target_thread_id: Some(ref audited_target),
                    ..
                } if action == audit_action && audited_target == &target_thread_id
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    assert!(matches!(
        prompt_audit,
        ThreadItem::UserAgentControl {
            authored_selector: Some(ref selector),
            prompt_preview: Some(ref prompt_preview),
            resumed_target: true,
            status: UserAgentControlStatus::Succeeded,
            ..
        } if selector == "2" && prompt_preview == prompt
    ));

    resumed_turn.expect(1).await;
    Ok(())
}

#[tokio::test]
async fn max_depth_agent_can_observe_and_resume_existing_same_root_target_in_v1_and_v2()
-> Result<()> {
    for multi_agent_v2 in [false, true] {
        max_depth_agent_can_observe_and_resume_existing_same_root_target(multi_agent_v2).await?;
    }
    Ok(())
}

async fn max_depth_agent_can_observe_and_resume_existing_same_root_target(
    multi_agent_v2: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config
        .with_root_config("[agents]\nmax_depth = 2")
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;

    let first_child: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: first_child_id,
        ..
    } = agent_control_outcome(first_child)
    else {
        panic!("first child should spawn");
    };
    let second_child: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: first_child_id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: max_depth_child_id,
        ..
    } = agent_control_outcome(second_child)
    else {
        panic!("max-depth child should spawn");
    };
    let beyond_depth: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: beyond_depth_id,
        ..
    } = agent_control_outcome(beyond_depth)
    else {
        panic!("explicit user spawn should exceed the autonomous model depth budget");
    };
    let beyond_depth_thread = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: beyond_depth_id.clone(),
            include_turns: false,
        })
        .await?
        .thread;
    assert!(matches!(
        beyond_depth_thread.source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 3,
            ..
        }) if parent_thread_id.to_string() == max_depth_child_id
    ));
    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id.clone(),
                authored_selector: Some(beyond_depth_id.clone()),
                action: AgentControlAction::Close {
                    target: beyond_depth_id,
                },
            },
        })
        .await?;

    let foreign_root = app.start_thread(ThreadStartParams::default()).await?;
    let foreign_child: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: foreign_root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: foreign_child_id,
        ..
    } = agent_control_outcome(foreign_child)
    else {
        panic!("foreign child should spawn");
    };
    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: foreign_root.thread.id,
                authored_selector: Some(foreign_child_id.clone()),
                action: AgentControlAction::Close {
                    target: foreign_child_id.clone(),
                },
            },
        })
        .await?;
    let adopted_beyond_depth: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id.clone(),
                authored_selector: Some(foreign_child_id.clone()),
                action: AgentControlAction::Resume {
                    target: foreign_child_id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(adopted_beyond_depth),
        AgentControlOutcome::Resumed {
            target_thread_id: ref adopted_id,
            post_commit_warning: None,
            ..
        } if adopted_id == &foreign_child_id
    ));
    let adopted_beyond_depth_thread = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: foreign_child_id.clone(),
            include_turns: false,
        })
        .await?
        .thread;
    assert!(matches!(
        adopted_beyond_depth_thread.source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 3,
            ..
        }) if parent_thread_id.to_string() == max_depth_child_id
    ));
    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id.clone(),
                authored_selector: Some(foreign_child_id.clone()),
                action: AgentControlAction::Close {
                    target: foreign_child_id,
                },
            },
        })
        .await?;

    let sibling: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: sibling_id,
        ..
    } = agent_control_outcome(sibling)
    else {
        panic!("sibling should spawn");
    };

    let observed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id.clone(),
                authored_selector: Some(sibling_id.clone()),
                action: AgentControlAction::Resume {
                    target: sibling_id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(observed),
        AgentControlOutcome::Resumed {
            target_thread_id: ref observed_id,
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
            ..
        } if observed_id == &sibling_id
    ));

    let _: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(sibling_id.clone()),
                action: AgentControlAction::Close {
                    target: sibling_id.clone(),
                },
            },
        })
        .await?;
    let resumed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: max_depth_child_id,
                authored_selector: Some(sibling_id.clone()),
                action: AgentControlAction::Resume {
                    target: sibling_id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(resumed),
        AgentControlOutcome::Resumed {
            target_thread_id: ref resumed_id,
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
            ..
        } if resumed_id == &sibling_id
    ));
    Ok(())
}

#[tokio::test]
async fn user_control_adopts_a_stored_standalone_rollout_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        user_control_adopts_a_stored_standalone_rollout(multi_agent_v2).await?;
    }
    Ok(())
}

#[tokio::test]
async fn adoption_reports_committed_ownership_when_watcher_setup_fails_in_v1_and_v2() -> Result<()>
{
    for multi_agent_v2 in [false, true] {
        adoption_reports_committed_ownership_when_watcher_setup_fails(multi_agent_v2).await?;
    }
    Ok(())
}

async fn adoption_reports_committed_ownership_when_watcher_setup_fails(
    multi_agent_v2: bool,
) -> Result<()> {
    const FOREIGN_SEED_PROMPT: &str = "materialize rollout for degraded adoption";

    let server = responses::start_mock_server().await;
    let foreign_seed_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(FOREIGN_SEED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-degraded-adoption-seed"),
            responses::ev_assistant_message(
                "msg-degraded-adoption-seed",
                "standalone rollout ready",
            ),
            responses::ev_completed("resp-degraded-adoption-seed"),
        ]),
    )
    .await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri()).with_root_config(&format!(
        r#"experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}"#
    ));
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;
    let foreign = app.start_thread(ThreadStartParams::default()).await?;
    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: foreign.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: FOREIGN_SEED_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == foreign.thread.id {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    foreign_seed_turn.expect(1).await;
    let _: ThreadArchiveResponse = app
        .request(|request_id| ClientRequest::ThreadArchive {
            request_id,
            params: ThreadArchiveParams {
                thread_id: foreign.thread.id.clone(),
            },
        })
        .await?;

    store
        .fail_agent_response_observation_flushes_after(
            /*successful_flushes*/ 0, /*failed_flushes*/ 1,
        )
        .await;
    let degraded: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(foreign.thread.id.clone()),
                action: AgentControlAction::Resume {
                    target: foreign.thread.id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Resumed {
        target_thread_id,
        agent_ref,
        nickname,
        observation_binding,
        post_commit_warning,
    } = agent_control_outcome(degraded)
    else {
        panic!("degraded adoption should report its committed resume");
    };
    assert_eq!(target_thread_id, foreign.thread.id);
    assert_eq!(agent_ref.as_deref(), Some("2"));
    assert!(nickname.is_some());
    assert_eq!(observation_binding, None);
    let warning = post_commit_warning.context("committed adoption warning should be present")?;
    assert!(warning.contains("agent is now owned by this root"));
    assert!(warning.contains("failed to persist initial response observation state"));

    let retry: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(foreign.thread.id.clone()),
                action: AgentControlAction::Resume {
                    target: foreign.thread.id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(retry),
        AgentControlOutcome::Resumed {
            target_thread_id: foreign.thread.id,
            agent_ref,
            nickname,
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
        }
    );

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

async fn user_control_adopts_a_stored_standalone_rollout(multi_agent_v2: bool) -> Result<()> {
    const FOREIGN_SEED_PROMPT: &str = "materialize the standalone rollout";
    const ADOPTED_PROMPT: &str = "work after explicit user adoption";

    let server = responses::start_mock_server().await;
    let foreign_seed_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(FOREIGN_SEED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-adoption-seed"),
            responses::ev_assistant_message(
                "msg-user-control-adoption-seed",
                "standalone rollout ready",
            ),
            responses::ev_completed("resp-user-control-adoption-seed"),
        ]),
    )
    .await;
    let adopted_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(ADOPTED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-adoption-work"),
            responses::ev_assistant_message(
                "msg-user-control-adoption-work",
                "adopted rollout finished",
            ),
            responses::ev_completed("resp-user-control-adoption-work"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let foreign = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: foreign.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: FOREIGN_SEED_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == foreign.thread.id {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    foreign_seed_turn.expect(1).await;

    let _: ThreadArchiveResponse = app
        .request(|request_id| ClientRequest::ThreadArchive {
            request_id,
            params: ThreadArchiveParams {
                thread_id: foreign.thread.id.clone(),
            },
        })
        .await?;

    let resumed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(foreign.thread.id.clone()),
                action: AgentControlAction::Resume {
                    target: foreign.thread.id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let adopted_nickname = match agent_control_outcome(resumed) {
        AgentControlOutcome::Resumed {
            target_thread_id,
            agent_ref: Some(ref agent_ref),
            nickname: Some(nickname),
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
        } if target_thread_id == foreign.thread.id && agent_ref == "2" => nickname,
        other => panic!("unexpected standalone adoption response: {other:?}"),
    };
    assert!(
        !adopted_nickname.eq_ignore_ascii_case(codex_protocol::MAIN_AGENT_NICKNAME),
        "an adopted foreign root should receive a generated child nickname"
    );
    let adoption_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Resume,
                    ..
                }
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    assert!(matches!(
        adoption_audit,
        ThreadItem::UserAgentControl {
            action: AuditAgentControlAction::Resume,
            target_thread_id: Some(ref target_thread_id),
            previous_owner_session_id: None,
            new_owner_session_id: Some(ref new_owner_session_id),
            status: UserAgentControlStatus::Succeeded,
            ..
        } if target_thread_id == &foreign.thread.id
            && new_owner_session_id == &root.thread.id
    ));
    let adopted_thread = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: foreign.thread.id.clone(),
            include_turns: false,
        })
        .await?
        .thread;
    assert_eq!(adopted_thread.session_id, root.thread.id);
    assert_eq!(
        adopted_thread.parent_thread_id.as_deref(),
        Some(root.thread.id.as_str())
    );
    assert_eq!(
        adopted_thread.agent_nickname.as_deref(),
        Some(adopted_nickname.as_str())
    );
    assert!(
        matches!(
            adopted_thread.source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ),
        "adopted standalone rollout must have a live child source"
    );
    let aliases: codex_app_server_protocol::AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: codex_app_server_protocol::AgentAliasListParams {
                root_thread_id: root.thread.id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(
        aliases
            .data
            .iter()
            .find(|alias| alias.thread_id == foreign.thread.id)
            .map(|alias| (alias.agent_ref.as_str(), alias.nickname.as_deref())),
        Some(("2", Some(adopted_nickname.as_str())))
    );

    let prompted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::ReservedPrompt {
                    target: foreign.thread.id.clone(),
                    input: vec![UserInput::Text {
                        text: ADOPTED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(prompted),
        AgentControlOutcome::ReservedPrompted {
            target_thread_id,
            ..
        } if target_thread_id == foreign.thread.id
    ));
    adopted_turn.expect(1).await;
    Ok(())
}

#[tokio::test]
async fn user_control_adoption_records_the_previous_owner_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        user_control_adoption_records_the_previous_owner(multi_agent_v2).await?;
    }
    Ok(())
}

async fn user_control_adoption_records_the_previous_owner(multi_agent_v2: bool) -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if multi_agent_v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let previous_owner = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let new_owner = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: previous_owner.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn an idle child");
    };
    let foreign_prompt: Result<AgentControlResponse, _> = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: new_owner.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::Prompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: "must not cross the live ownership boundary".to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: None,
                },
            },
        })
        .await;
    let error = foreign_prompt.expect_err("a live child must remain exclusive to its current root");
    assert!(
        error.to_string().contains("is not controlled by this root"),
        "unexpected foreign-child error: {error}"
    );
    let foreign_resume: Result<AgentControlResponse, _> = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: new_owner.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::Resume {
                    target: target_thread_id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await;
    let error = foreign_resume.expect_err("a live child must not be adopted in place");
    assert!(
        error.to_string().contains("is live under another root"),
        "unexpected live-adoption error: {error}"
    );

    let closed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: previous_owner.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::Close {
                    target: target_thread_id.clone(),
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(closed),
        AgentControlOutcome::Closed {
            target_thread_id: target_thread_id.clone(),
        }
    );

    let resumed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: new_owner.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::Resume {
                    target: target_thread_id.clone(),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(resumed),
        AgentControlOutcome::Resumed {
            target_thread_id: ref resumed_thread_id,
            agent_ref: Some(ref agent_ref),
            nickname: Some(_),
            observation_binding: Some(AgentObservationBinding::NextTurn),
            post_commit_warning: None,
        } if resumed_thread_id == &target_thread_id && agent_ref == "2"
    ));

    let adoption_audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Resume,
                    target_thread_id: Some(ref audited_target),
                    ..
                } if audited_target == &target_thread_id
            ) {
                return Ok::<_, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    assert!(matches!(
        adoption_audit,
        ThreadItem::UserAgentControl {
            previous_owner_session_id: Some(ref previous_owner_session_id),
            new_owner_session_id: Some(ref new_owner_session_id),
            status: UserAgentControlStatus::Succeeded,
            ..
        } if previous_owner_session_id == &previous_owner.thread.id
            && new_owner_session_id == &new_owner.thread.id
    ));
    let adopted_thread = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: target_thread_id.clone(),
            include_turns: false,
        })
        .await?
        .thread;
    assert_eq!(adopted_thread.session_id, new_owner.thread.id);
    let aliases_from_adopted_child: codex_app_server_protocol::AgentAliasListResponse = app
        .request(|request_id| ClientRequest::AgentAliasList {
            request_id,
            params: codex_app_server_protocol::AgentAliasListParams {
                root_thread_id: target_thread_id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(
        aliases_from_adopted_child
            .data
            .iter()
            .map(|alias| alias.thread_id.as_str())
            .collect::<Vec<_>>(),
        vec![new_owner.thread.id.as_str(), target_thread_id.as_str()]
    );
    Ok(())
}

#[tokio::test]
async fn user_control_keeps_v2_identity_and_durable_response_observation() -> Result<()> {
    const SPAWN_PROMPT: &str = "user-authored V2 spawn task";
    const FOLLOW_UP_PROMPT: &str = "user-authored V2 follow-up task";
    const ROOT_CHECK_PROMPT: &str = "check the V2 source context";

    let server = responses::start_mock_server().await;
    let spawned_child_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(SPAWN_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-v2"),
            responses::ev_assistant_message("msg-user-control-v2", "V2 child done"),
            responses::ev_completed("resp-user-control-v2"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;
    let root_check_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(ROOT_CHECK_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-v2-root-check"),
            responses::ev_assistant_message(
                "msg-user-control-v2-root-check",
                "V2 source context checked",
            ),
            responses::ev_completed("resp-user-control-v2-root-check"),
        ]),
    )
    .await;
    let prompted_child_turn = responses::mount_sse_once_match(
        &server,
        |request| request.body_contains_text(FOLLOW_UP_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-user-control-v2-follow-up"),
            responses::ev_assistant_message("msg-user-control-v2-follow-up", "V2 follow-up done"),
            responses::ev_completed("resp-user-control-v2-follow-up"),
        ])
        .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::MultiAgentV2)
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;

    let spawned: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: Some(vec![UserInput::Text {
                        text: SPAWN_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Wake),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id,
        agent_ref,
        ..
    } = agent_control_outcome(spawned)
    else {
        panic!("user control should spawn a V2 child");
    };
    assert_eq!(agent_ref.as_deref(), Some("2"));

    let observed: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Observe {
                    target: "2".to_string(),
                    response_handling: AgentObservationMode::Presentation,
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(observed),
        AgentControlOutcome::Observed {
            target_thread_id: target_thread_id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Wake,
            response_handling: AgentFinalResponseHandling::Presentation,
            binding: AgentObservationBinding::ActiveTurn,
        }
    );
    let observed_again: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Observe {
                    target: "2".to_string(),
                    response_handling: AgentObservationMode::Passive,
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(observed_again),
        AgentControlOutcome::Observed {
            target_thread_id: target_thread_id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Presentation,
            response_handling: AgentFinalResponseHandling::Passive,
            binding: AgentObservationBinding::ActiveTurn,
        }
    );
    spawned_child_turn.expect(1).await;
    let spawned_request = spawned_child_turn.single_request();
    assert!(spawned_request.body_contains_text(SPAWN_PROMPT));
    assert!(spawned_request.inputs_of_type("agent_message").is_empty());

    let idle_spawn: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    role: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id: idle_thread_id,
        agent_ref: idle_agent_ref,
        ..
    } = agent_control_outcome(idle_spawn)
    else {
        panic!("user control should spawn an idle V2 child");
    };
    assert_eq!(idle_agent_ref.as_deref(), Some("3"));

    let prompted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: None,
                action: AgentControlAction::ReservedPrompt {
                    target: idle_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: FOLLOW_UP_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(prompted),
        AgentControlOutcome::ReservedPrompted {
            target_thread_id: ref prompted_thread_id,
            ..
        } if prompted_thread_id == &idle_thread_id
    ));
    let observed_first_prompt: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("3".to_string()),
                action: AgentControlAction::Observe {
                    target: "3".to_string(),
                    response_handling: AgentObservationMode::Passive,
                },
            },
        })
        .await?;
    assert_eq!(
        agent_control_outcome(observed_first_prompt),
        AgentControlOutcome::Observed {
            target_thread_id: idle_thread_id.clone(),
            previous_response_handling: AgentFinalResponseHandling::Presentation,
            response_handling: AgentFinalResponseHandling::Passive,
            binding: AgentObservationBinding::ActiveTurn,
        }
    );
    prompted_child_turn.expect(1).await;
    let prompted_request = prompted_child_turn.single_request();
    assert!(prompted_request.body_contains_text(FOLLOW_UP_PROMPT));
    assert!(prompted_request.inputs_of_type("agent_message").is_empty());

    let child_sources = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let listed: ThreadListResponse = app
                .request(|request_id| ClientRequest::ThreadList {
                    request_id,
                    params: ThreadListParams {
                        cursor: None,
                        limit: Some(10),
                        sort_key: None,
                        sort_direction: None,
                        model_providers: None,
                        source_kinds: None,
                        archived: None,
                        section_id: None,
                        project_id: None,
                        cwd: None,
                        use_state_db_only: true,
                        search_term: None,
                        parent_thread_id: Some(root.thread.id.clone()),
                        ancestor_thread_id: None,
                    },
                })
                .await?;
            let children = listed
                .data
                .into_iter()
                .filter(|thread| thread.id == target_thread_id || thread.id == idle_thread_id)
                .map(|thread| thread.source)
                .collect::<Vec<_>>();
            if children.len() == 2 {
                return Ok::<_, anyhow::Error>(children);
            }
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .context("user-controlled V2 child is missing from thread/list")??;
    assert!(child_sources.into_iter().all(|source| matches!(
        source,
        SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            agent_path: Some(path),
            ..
        }) if path.as_str().starts_with("/root/user-")
    )));

    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: root.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: ROOT_CHECK_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    root_check_turn.expect(1).await;
    let root_request = root_check_turn.single_request();
    let task_contexts = root_request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.contains("<user_agent_task>"))
        .collect::<Vec<_>>();
    assert_eq!(task_contexts.len(), 2);
    assert!(
        task_contexts
            .iter()
            .all(|task| task.contains(r#""agent_path":"/root/user-"#))
    );
    assert!(root_request.body_contains_text(SPAWN_PROMPT));
    assert!(root_request.body_contains_text(FOLLOW_UP_PROMPT));

    Ok(())
}
