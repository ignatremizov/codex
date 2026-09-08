use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn explicit_observer_preserves_direction_and_issuer_audit() -> Result<()> {
    for multi_agent_v2 in [false, true] {
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
        let main = app.start_thread(ThreadStartParams::default()).await?;
        let spawned: AgentControlResponse = app
            .request(|request_id| ClientRequest::AgentControl {
                request_id,
                params: AgentControlParams {
                    source_thread_id: main.thread.id.clone(),
                    authored_selector: None,
                    action: AgentControlAction::Spawn {
                        task: None,
                        role: None,
                        model: None,
                        reasoning_effort: None,
                        input: None,
                        fork_mode: AgentForkMode::None,
                        response_handling: Some(AgentResponseHandling::Wake),
                    },
                },
            })
            .await?;
        let AgentControlOutcome::Spawned {
            target_thread_id: child_id,
            agent_ref,
            nickname,
            ..
        } = agent_control_outcome(spawned)
        else {
            anyhow::bail!("expected an idle child");
        };
        let agent_ref = agent_ref.context("child ref")?;
        let nickname = nickname.context("child nickname")?;
        let authored_ref = format!("ref:{agent_ref}");
        let authored_nickname = format!("\"{nickname}\"");

        // No reverse subscription exists yet. Observe must not create one.
        let request_id = app
            .send_raw_request(
                "agent/control",
                Some(serde_json::json!({
                    "sourceThreadId": main.thread.id,
                    "action": {
                        "type": "observe", "target": "Main", "observer": child_id,
                        "responseHandling": "presentation"
                    }
                })),
            )
            .await?;
        let error = app
            .read_stream_until_error_message(RequestId::Integer(request_id))
            .await?;
        assert!(
            error
                .error
                .message
                .contains("no active or pending response observation")
        );

        let resumed: AgentControlResponse = app
            .request(|request_id| ClientRequest::AgentControl {
                request_id,
                params: AgentControlParams {
                    source_thread_id: child_id.clone(),
                    authored_selector: None,
                    action: AgentControlAction::Resume {
                        target: "Main".to_string(),
                        task: None,
                        response_handling: Some(AgentResponseHandling::Wake),
                    },
                },
            })
            .await?;
        assert!(matches!(
            agent_control_outcome(resumed),
            AgentControlOutcome::Resumed { .. }
        ));

        for (target, observer, authored_observer_selector, mode, expected) in [
            (
                "Main".to_string(),
                Some(agent_ref),
                Some(authored_ref.clone()),
                AgentObservationMode::Presentation,
                AgentControlOutcome::Observed {
                    target_thread_id: main.thread.id.clone(),
                    observer_thread_id: child_id.clone(),
                    previous_response_handling: AgentFinalResponseHandling::Wake,
                    response_handling: AgentFinalResponseHandling::Presentation,
                    binding: AgentObservationBinding::NextTurn,
                },
            ),
            (
                child_id.clone(),
                None,
                Some(authored_nickname.clone()),
                AgentObservationMode::Wake,
                AgentControlOutcome::Observed {
                    target_thread_id: child_id.clone(),
                    observer_thread_id: main.thread.id.clone(),
                    previous_response_handling: AgentFinalResponseHandling::Wake,
                    response_handling: AgentFinalResponseHandling::Wake,
                    binding: AgentObservationBinding::NextTurn,
                },
            ),
            (
                "Main".to_string(),
                Some(nickname),
                Some(authored_nickname.clone()),
                AgentObservationMode::Passive,
                AgentControlOutcome::Observed {
                    target_thread_id: main.thread.id.clone(),
                    observer_thread_id: child_id.clone(),
                    previous_response_handling: AgentFinalResponseHandling::Presentation,
                    response_handling: AgentFinalResponseHandling::Passive,
                    binding: AgentObservationBinding::NextTurn,
                },
            ),
        ] {
            let response: AgentControlResponse = app
                .request(|request_id| ClientRequest::AgentControl {
                    request_id,
                    params: AgentControlParams {
                        source_thread_id: main.thread.id.clone(),
                        authored_selector: Some(target.clone()),
                        action: AgentControlAction::Observe {
                            target,
                            observer,
                            authored_observer_selector,
                            response_handling: mode,
                        },
                    },
                })
                .await?;
            assert_eq!(agent_control_outcome(response), expected);
        }
        // An older client omitting the field altogether still updates Main's own subscription.
        let request_id = app
            .send_raw_request(
                "agent/control",
                Some(serde_json::json!({
                    "sourceThreadId": main.thread.id,
                    "action": {
                        "type": "observe", "target": child_id, "responseHandling": "wake"
                    }
                })),
            )
            .await?;
        let omitted: AgentControlResponse = app.read_response(request_id).await?;
        assert_eq!(
            agent_control_outcome(omitted),
            AgentControlOutcome::Observed {
                target_thread_id: child_id.clone(),
                observer_thread_id: main.thread.id.clone(),
                previous_response_handling: AgentFinalResponseHandling::Wake,
                response_handling: AgentFinalResponseHandling::Wake,
                binding: AgentObservationBinding::NextTurn,
            }
        );
        let foreign = app.start_thread(ThreadStartParams::default()).await?;
        let request_id = app
            .send_raw_request(
                "agent/control",
                Some(serde_json::json!({
                    "sourceThreadId": main.thread.id,
                    "action": {
                        "type": "observe", "target": child_id, "observer": foreign.thread.id,
                        "responseHandling": "passive"
                    }
                })),
            )
            .await?;
        app.read_stream_until_error_message(RequestId::Integer(request_id))
            .await?;
        // A valid raw token must not rescue an invalid normalized routing selector.
        let request_id = app
            .send_raw_request(
                "agent/control",
                Some(serde_json::json!({
                    "sourceThreadId": main.thread.id,
                    "action": {
                        "type": "observe", "target": child_id, "observer": "unknown-observer",
                        "authoredObserverSelector": "Main",
                        "responseHandling": "passive"
                    }
                })),
            )
            .await?;
        app.read_stream_until_error_message(RequestId::Integer(request_id))
            .await?;
        let read: ThreadReadResponse = app
            .request(|request_id| ClientRequest::ThreadRead {
                request_id,
                params: ThreadReadParams {
                    thread_id: main.thread.id.clone(),
                    include_turns: true,
                },
            })
            .await?;
        let audit = read
            .thread
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter_map(|item| {
                if let ThreadItem::UserAgentControl {
                    action: AuditAgentControlAction::Observe,
                    observer_thread_id,
                    authored_observer_selector,
                    status,
                    ..
                } = item
                {
                    Some((
                        observer_thread_id.clone(),
                        authored_observer_selector.clone(),
                        *status,
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            audit,
            vec![
                (
                    Some(child_id.clone()),
                    Some(child_id.clone()),
                    UserAgentControlStatus::Failed
                ),
                (
                    Some(child_id.clone()),
                    Some(authored_ref),
                    UserAgentControlStatus::Succeeded
                ),
                (
                    Some(main.thread.id.clone()),
                    None,
                    UserAgentControlStatus::Succeeded
                ),
                (
                    Some(child_id.clone()),
                    Some(authored_nickname),
                    UserAgentControlStatus::Succeeded
                ),
                (
                    Some(main.thread.id.clone()),
                    None,
                    UserAgentControlStatus::Succeeded
                ),
                (
                    Some(foreign.thread.id.clone()),
                    Some(foreign.thread.id),
                    UserAgentControlStatus::Failed
                ),
                (
                    None,
                    Some("Main".to_string()),
                    UserAgentControlStatus::Failed
                ),
            ]
        );
        let requests = server.received_requests().await.context("mock requests")?;
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().ends_with("/responses"))
        );
    }
    Ok(())
}
