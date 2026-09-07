use super::*;
use codex_app_server_protocol::AgentControlAction;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentControlParams;
use codex_app_server_protocol::AgentControlResponse;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::AgentReplyRouteMode;
use codex_app_server_protocol::AgentResponseHandling;

async fn control(
    app: &mut TestAppServer,
    source: &str,
    action: AgentControlAction,
) -> Result<AgentControlOutcome> {
    let response: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: source.to_string(),
                authored_selector: None,
                action,
            },
        })
        .await?;
    assert_eq!(response.audit_warning, None);
    Ok(response.outcome)
}

async fn spawn_idle(app: &mut TestAppServer, source: &str) -> Result<String> {
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = control(
        app,
        source,
        AgentControlAction::Spawn {
            task: None,
            role: None,
            model: None,
            reasoning_effort: None,
            input: None,
            fork_mode: AgentForkMode::None,
            response_handling: Some(AgentResponseHandling::Presentation),
        },
    )
    .await?
    else {
        anyhow::bail!("expected an idle child");
    };
    Ok(target_thread_id)
}

#[test_case::test_case(false; "root_replacement")]
#[test_case::test_case(true; "nested_supervisor_replacement")]
#[tokio::test]
async fn revert_preserves_policy_on_both_sides_of_the_conversation_cutoff(
    nested: bool,
) -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut app).await?;
    // start_thread delegates to send_thread_start_request_with_auto_env; executor selection is
    // retained across the revert, without host-native filesystem assumptions.
    let root = app
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?
        .thread
        .id;
    let supervisor = spawn_idle(&mut app, &root).await?;
    let worker = spawn_idle(&mut app, &supervisor).await?;
    let reverting = if nested { &supervisor } else { &root };
    control(
        &mut app,
        &root,
        AgentControlAction::SubtreeMessaging {
            mode: AgentReplyRouteMode::Enabled,
        },
    )
    .await?;
    control(
        &mut app,
        &root,
        AgentControlAction::ReplyRoute {
            target: worker.clone(),
            recipient: Some(reverting.clone()),
            mode: AgentReplyRouteMode::Enabled,
        },
    )
    .await?;
    let mut turns = Vec::new();
    for prompt in ["retained work", "work removed by revert"] {
        let completed = app
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: reverting.clone(),
                input: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        turns.push(completed.turn.id);
    }
    // These live settings are newer than the conversation boundary being restored.
    control(
        &mut app,
        &supervisor,
        AgentControlAction::SubtreeMessaging {
            mode: AgentReplyRouteMode::Disabled,
        },
    )
    .await?;
    control(
        &mut app,
        &root,
        AgentControlAction::ReplyRoute {
            target: worker.clone(),
            recipient: Some(reverting.clone()),
            mode: AgentReplyRouteMode::Disabled,
        },
    )
    .await?;
    let _: ThreadRevertResponse = app
        .request(|request_id| ClientRequest::ThreadRevert {
            request_id,
            params: ThreadRevertParams {
                thread_id: reverting.clone(),
                before_turn_id: turns[1].clone(),
            },
        })
        .await?;
    for (source, mode) in [
        (&root, AgentReplyRouteMode::Enabled),
        (&supervisor, AgentReplyRouteMode::Disabled),
    ] {
        assert_eq!(
            control(
                &mut app,
                source,
                AgentControlAction::SubtreeMessaging { mode }
            )
            .await?,
            AgentControlOutcome::SubtreeMessagingChanged {
                root_thread_id: source.clone(),
                previous_mode: Some(mode),
                mode,
            }
        );
    }
    assert_eq!(
        control(
            &mut app,
            &root,
            AgentControlAction::ReplyRoute {
                target: worker.clone(),
                recipient: Some(reverting.clone()),
                mode: AgentReplyRouteMode::Disabled,
            }
        )
        .await?,
        AgentControlOutcome::ReplyRouteChanged {
            target_thread_id: worker,
            recipient_thread_id: reverting.clone(),
            previous_mode: Some(AgentReplyRouteMode::Disabled),
            mode: AgentReplyRouteMode::Disabled,
        }
    );
    let forked: ThreadForkResponse = app
        .request(|request_id| ClientRequest::ThreadFork {
            request_id,
            params: ThreadForkParams {
                thread_id: reverting.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(
        control(
            &mut app,
            &forked.thread.id,
            AgentControlAction::SubtreeMessaging {
                mode: AgentReplyRouteMode::Enabled,
            }
        )
        .await?,
        AgentControlOutcome::SubtreeMessagingChanged {
            root_thread_id: forked.thread.id,
            previous_mode: None,
            mode: AgentReplyRouteMode::Enabled,
        }
    );
    app.shutdown_gracefully().await?;

    // Process restart must not turn the preserved audit records into live permission.
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut app).await?;
    let _: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: root.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(
        control(
            &mut app,
            &root,
            AgentControlAction::SubtreeMessaging {
                mode: AgentReplyRouteMode::Enabled,
            }
        )
        .await?,
        AgentControlOutcome::SubtreeMessagingChanged {
            root_thread_id: root,
            previous_mode: None,
            mode: AgentReplyRouteMode::Enabled,
        }
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn failed_revert_reload_preserves_explicit_routes_without_a_subtree_default() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut app).await?;
    let root = app
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?
        .thread
        .id;
    let worker = spawn_idle(&mut app, &root).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: root.clone(),
        input: vec![UserInput::Text {
            text: "materialize history before an invalid revert".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    control(
        &mut app,
        &root,
        AgentControlAction::ReplyRoute {
            target: worker.clone(),
            recipient: None,
            mode: AgentReplyRouteMode::Enabled,
        },
    )
    .await?;
    let request = app
        .send_raw_request(
            "thread/revert",
            Some(serde_json::to_value(ThreadRevertParams {
                thread_id: root.clone(),
                before_turn_id: "nonexistent-turn".to_string(),
            })?),
        )
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request)),
    )
    .await??;
    assert_eq!(
        control(
            &mut app,
            &root,
            AgentControlAction::ReplyRoute {
                target: worker.clone(),
                recipient: None,
                mode: AgentReplyRouteMode::Enabled,
            }
        )
        .await?,
        AgentControlOutcome::ReplyRouteChanged {
            target_thread_id: worker,
            recipient_thread_id: root,
            previous_mode: Some(AgentReplyRouteMode::Enabled),
            mode: AgentReplyRouteMode::Enabled,
        }
    );
    app.shutdown_gracefully().await?;
    Ok(())
}
