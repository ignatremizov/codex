//! Public ownership changes retain identity and history while stopping only an idle branch.

use super::*;
use codex_app_server_protocol::AgentAliasListParams;
use codex_app_server_protocol::AgentAliasListResponse;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::ThreadDetachParams;
use codex_app_server_protocol::ThreadDetachResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use pretty_assertions::assert_eq;
use test_case::test_case;

async fn spawn_idle(app: &mut TestAppServer, parent: &str, task: &str) -> Result<String> {
    let response: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: parent.into(),
                authored_selector: Some("new".into()),
                action: AgentControlAction::Spawn {
                    task: Some(task.into()),
                    role: None,
                    model: Some("gpt-5.5".into()),
                    reasoning_effort: Some(ReasoningEffort::High),
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = agent_control_outcome(response)
    else {
        anyhow::bail!("expected an idle child");
    };
    Ok(target_thread_id)
}

async fn aliases(app: &mut TestAppServer, root: &str) -> Result<AgentAliasListResponse> {
    app.request(|request_id| ClientRequest::AgentAliasList {
        request_id,
        params: AgentAliasListParams {
            root_thread_id: root.into(),
            cursor: None,
            limit: None,
        },
    })
    .await
}

#[test_case(false, ThreadHistoryMode::Legacy; "v1_legacy")]
#[test_case(false, ThreadHistoryMode::Paginated; "v1_paginated")]
#[test_case(true, ThreadHistoryMode::Legacy; "v2_legacy")]
#[test_case(true, ThreadHistoryMode::Paginated; "v2_paginated")]
#[tokio::test]
async fn detach_and_explicit_live_root_resume_round_trip(
    v2: bool,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = if v2 {
        config.enable_feature(Feature::MultiAgentV2)
    } else {
        config.disable_feature(Feature::MultiAgentV2)
    };
    config.write(home.path())?;
    write_models_cache(home.path()).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let root = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.5".into()),
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?;
    let child = spawn_idle(&mut app, &root.thread.id, "work").await?;
    let grandchild = spawn_idle(&mut app, &child, "check").await?;
    let sibling = spawn_idle(&mut app, &root.thread.id, "other").await?;
    let original = aliases(&mut app, &root.thread.id).await?;
    let seed = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("detach-seed"),
            responses::ev_assistant_message("detach-seed-final", "Retain this child history."),
            responses::ev_completed("detach-seed"),
        ]),
    )
    .await;
    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: child.clone(),
                input: vec![UserInput::Text {
                    text: "Seed the child before detaching.".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_thread_turn_completed(&mut app, &child).await?;
    assert!(
        seed.single_request()
            .body_json()
            .to_string()
            .contains("Seed the child")
    );
    let before: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: child.clone(),
                include_turns: true,
            },
        })
        .await?;

    let detached: ThreadDetachResponse = app
        .request(|request_id| ClientRequest::ThreadDetach {
            request_id,
            params: ThreadDetachParams {
                thread_id: child.clone(),
            },
        })
        .await?;
    assert_eq!(
        detached,
        ThreadDetachResponse {
            thread_id: child.clone(),
            previous_root_thread_id: root.thread.id.clone(),
            unloaded_thread_ids: vec![child.clone(), grandchild.clone()],
        },
    );
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(
        loaded
            .data
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([root.thread.id.clone(), sibling]),
    );
    let mut retired = original.clone();
    for alias in &mut retired.data {
        if alias.thread_id == child || alias.thread_id == grandchild {
            alias.state = AgentAliasState::Transferred;
        }
    }
    assert_eq!(aliases(&mut app, &root.thread.id).await?, retired);
    let independent = aliases(&mut app, &child).await?;
    assert_eq!(
        independent
            .data
            .iter()
            .map(|alias| (
                alias.thread_id.as_str(),
                alias.agent_ref.as_str(),
                alias.task_path.as_deref(),
                alias.state,
            ))
            .collect::<Vec<_>>(),
        vec![
            (child.as_str(), "1", Some("/root"), AgentAliasState::Active),
            (
                grandchild.as_str(),
                "2",
                Some("/root/check"),
                AgentAliasState::Active
            ),
        ],
    );
    let resumed: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: child.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(
        (resumed.thread.id, resumed.model, resumed.reasoning_effort),
        (child.clone(), "gpt-5.5".into(), Some(ReasoningEffort::High)),
    );
    let after: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: child.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(
        serde_json::to_value(after.thread.turns)?,
        serde_json::to_value(before.thread.turns)?
    );

    // No separate close is necessary for an idle independent root.
    let reattached: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(child.clone()),
                action: AgentControlAction::Resume {
                    target: child.clone(),
                    task: Some("work".into()),
                    response_handling: Some(AgentResponseHandling::Presentation),
                },
            },
        })
        .await?;
    assert!(matches!(
        agent_control_outcome(reattached),
        AgentControlOutcome::Resumed {
            target_thread_id, agent_ref: Some(agent_ref), task_path: Some(task), ..
        } if target_thread_id == child && agent_ref == "2" && task == "/root/work"
    ));
    assert_eq!(aliases(&mut app, &root.thread.id).await?, original);
    assert!(
        aliases(&mut app, &child)
            .await?
            .data
            .iter()
            .all(|alias| alias.state == AgentAliasState::Active)
    );
    Ok(())
}

#[tokio::test]
async fn busy_descendant_refuses_detach_without_interrupting_work() -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(server.uri()).write(home.path())?;
    write_models_cache(home.path()).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let root = app.start_thread(ThreadStartParams::default()).await?;
    let child = spawn_idle(&mut app, &root.thread.id, "work").await?;
    let grandchild = spawn_idle(&mut app, &child, "check").await?;
    let original = aliases(&mut app, &root.thread.id).await?;
    let (release, gate) = oneshot::channel();
    let mut work = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: responses::sse(vec![
                    responses::ev_response_created("busy-descendant"),
                    responses::ev_assistant_message("busy-descendant-final", "Work completed."),
                    responses::ev_completed("busy-descendant"),
                ]),
            }],
        )
        .await;
    let _: codex_app_server_protocol::TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: grandchild.clone(),
                input: vec![UserInput::Text {
                    text: "Keep this descendant busy.".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, work.wait_for_request()).await?;
    let request = app
        .send_raw_request("thread/detach", Some(serde_json::json!({"threadId":child})))
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(request))
        .await?;
    assert!(
        error.error.message.contains("active or queued work"),
        "{:?}",
        error.error
    );
    assert_eq!(aliases(&mut app, &root.thread.id).await?, original);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("busy work was interrupted"))?;
    wait_for_thread_turn_completed(&mut app, &grandchild).await?;
    // Earlier idle reservations were released when the busy descendant refused.
    let _: ThreadDetachResponse = app
        .request(|request_id| ClientRequest::ThreadDetach {
            request_id,
            params: ThreadDetachParams {
                thread_id: child.clone(),
            },
        })
        .await?;
    server.shutdown().await;
    Ok(())
}
