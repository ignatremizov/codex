//! Question replies use the captured server and source/requester UUIDs across focus changes.

use super::*;
use crate::agent_question::AgentQuestionAnswer;
use crate::agent_question::AgentQuestionDelivery;
use crate::app_server_session::AppServerSessionEvent;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_protocol::items::AgentMessageDelivery;
use codex_protocol::items::AsyncUserInputQuestion;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;

fn question_event(server: &AppServerSession, requester: ThreadId) -> AppServerSessionEvent {
    AppServerSessionEvent {
        backend_id: server.backend_id(),
        event: AppServerEvent::ServerNotification(Box::new(ServerNotification::ItemCompleted(
            ItemCompletedNotification::new(
                requester.to_string(),
                "question-turn".into(),
                ThreadItemPayload::AgentMessage {
                    id: "async-question:q1:child-call".into(),
                    text: "Which deployment?".into(),
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    attribution: None,
                    input: None,
                    memory_citation: None,
                    inter_agent_source: None,
                    delivery: Some(AgentMessageDelivery::Async),
                    questions: Some(vec![AsyncUserInputQuestion {
                        title: "Which deployment?".into(),
                        options: None,
                    }]),
                },
                /*started_at_ms*/ None,
                0,
            ),
        ))),
    }
}

async fn publish_question(app: &mut App, server: &AppServerSession, requester: ThreadId) {
    let source = app.primary_thread_id.expect("question owner");
    // Exercise the real first-seen order without manually priming navigation metadata.
    assert!(!app.agent_navigation.is_subagent(requester));
    let spawn = serde_json::from_value(serde_json::json!({
        "type": "collabAgentToolCall",
        "id": "spawn-questioner",
        "tool": "spawnAgent",
        "status": "completed",
        "senderThreadId": source.to_string(),
        "receiverThreadIds": [requester.to_string()],
        "receiverAgents": [{
            "threadId": requester.to_string(),
            "agentNickname": "Reviewer",
            "agentRole": "worker"
        }],
        "agentsStates": {}
    }))
    .expect("canonical spawn item");
    app.handle_backend_event(
        server,
        AppServerSessionEvent {
            backend_id: server.backend_id(),
            event: AppServerEvent::ServerNotification(Box::new(ServerNotification::ItemCompleted(
                ItemCompletedNotification::new(
                    source.to_string(),
                    "spawn-turn".into(),
                    spawn,
                    /*started_at_ms*/ None,
                    0,
                ),
            ))),
        },
    )
    .await;
    app.handle_backend_event(server, question_event(server, requester))
        .await;
}

fn compose_answer(
    app: &mut App,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    text: &str,
    delivery: AgentQuestionDelivery,
) -> AgentQuestionAnswer {
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    app.chat_widget.handle_paste(text.into());
    app.chat_widget
        .handle_key_event(KeyEvent::from(match delivery {
            AgentQuestionDelivery::Direct => KeyCode::Enter,
            AgentQuestionDelivery::Queued => KeyCode::Tab,
        }));
    take_answer(events)
}

fn take_answer(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> AgentQuestionAnswer {
    std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::AnswerAgentQuestion(answer) => Some(answer),
            _ => None,
        })
        .expect("question answer event")
}

#[tokio::test]
async fn a_question_before_spawn_metadata_uses_its_current_server_parent() -> Result<()> {
    for known_picker_entry in [false, true] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        set_test_initial_prompt(&mut app, String::new());
        let model_server = MockServer::start().await;
        configure_agent_prompt_model_server(&mut app, &model_server);
        app.config.model_provider_id = "agent-question-test".into();
        app.config.model_providers.insert(
            app.config.model_provider_id.clone(),
            app.config.model_provider.clone(),
        );
        app.config.features.enable(Feature::Collab)?;
        app.config.features.disable(Feature::MultiAgentV2)?;
        let (mut server, requests, proxy) = start_recording_app_server_with_realtime_speech(
            &app.config,
            HistoryCapabilities::CurrentWithLiveNotifications,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            RecordingRequestBehavior {
                realtime: RealtimeRequestBehavior::Forward,
                agent_control: AgentControlRequestBehavior::Forward,
                blocked_older_turn_metadata: None,
            },
            LoaderOverrides::without_managed_config_for_tests(),
        )
        .await?;
        let started = server.start_thread(&app.config).await?;
        let root = started.session.thread_id;
        app.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        display_test_thread(&mut app, root);
        let requester = app
            .spawn_agent_from_command(
                &mut server,
                crate::app::SpawnAgentCommandArgs {
                    source_thread_id: root,
                    task: Some("questioner".into()),
                    role: None,
                    authored_selector: Some("new".into()),
                    model: None,
                    reasoning_effort: None,
                    prompt: None,
                    fork_mode: codex_app_server_protocol::AgentForkMode::None,
                    response_handling: Some(
                        codex_app_server_protocol::AgentResponseHandling::Presentation,
                    ),
                },
            )
            .await
            .expect("idle requester");
        app.agent_navigation.remove(requester);
        app.thread_event_channels.remove(&requester);
        if known_picker_entry {
            app.agent_navigation.upsert(
                requester,
                Some("Reviewer".into()),
                /*agent_role*/ None,
                /*is_closed*/ false,
            );
        }
        requests.lock().expect("recorder").clear();
        app.handle_backend_event(&server, question_event(&server, requester))
            .await;
        let answer = compose_answer(
            &mut app,
            &mut events,
            "Staging",
            AgentQuestionDelivery::Direct,
        );
        assert_eq!(
            (
                answer.binding.requester_thread_id,
                answer.binding.source_thread_id
            ),
            (requester, root),
        );
        assert!(
            recorded_params(&requests, "thread/read")
                .iter()
                .any(|params| params["threadId"] == requester.to_string())
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn answered_subagent_receives_literal_input_after_the_selected_thread_changes() -> Result<()>
{
    for (delivery, viewing_requester) in [
        (AgentQuestionDelivery::Direct, false),
        (AgentQuestionDelivery::Direct, true),
        (AgentQuestionDelivery::Queued, false),
        (AgentQuestionDelivery::Queued, true),
    ] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        set_test_initial_prompt(&mut app, String::new());
        let (mut server, requests, proxy) = start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
        let source = ThreadId::new();
        let requester = ThreadId::new();
        let selected = ThreadId::new();
        app.primary_thread_id = Some(source);
        display_test_thread(&mut app, if viewing_requester { requester } else { source });
        publish_question(&mut app, &server, requester).await;
        let answer = compose_answer(&mut app, &mut events, "!literal $answer", delivery);
        let expected_text = answer.text.clone();
        display_test_thread(&mut app, selected);
        app.chat_widget
            .restore_user_message_to_composer("Keep the selected thread's draft".into());
        requests.lock().unwrap().clear();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::AnswerAgentQuestion(answer.clone()),
        ))
        .await?;
        let recorded = requests.lock().unwrap().clone();
        assert_eq!(
            recorded
                .iter()
                .filter(|request| request.method == "agent/control")
                .filter_map(|request| request.params.clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!({
                "sourceThreadId": source.to_string(),
                "authoredSelector": serde_json::Value::Null,
                "action": {
                    "type": match delivery {
                        AgentQuestionDelivery::Direct => "prompt",
                        AgentQuestionDelivery::Queued => "queuedPrompt",
                    },
                    "target": requester.to_string(),
                    "input": [{"type": "text", "text": expected_text, "text_elements": []}],
                    "responseHandling": {
                        "commentary": false,
                        "finalResponse": "none",
                        "targetMessages": false,
                        "queueInput": false,
                    },
                },
            })]
        );
        assert!(
            recorded
                .iter()
                .all(|request| !matches!(request.method.as_str(), "turn/start" | "turn/steer"))
        );
        assert_eq!(app.current_displayed_thread_id(), Some(selected));
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "Keep the selected thread's draft"
        );
        assert!(!app.chat_widget.has_pending_agent_answer(&answer));
        // A delayed duplicate event cannot dispatch an already accepted answer again.
        app.answer_agent_question(&mut server, answer).await;
        assert_eq!(recorded_params(&requests, "agent/control").len(), 1);
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn backend_changes_and_refused_ownership_keep_the_bound_answer_for_retry() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    set_test_initial_prompt(&mut app, String::new());
    let (mut owner, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let (mut other, other_requests, other_proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source = ThreadId::new();
    let requester = ThreadId::new();
    app.primary_thread_id = Some(source);
    display_test_thread(&mut app, source);
    publish_question(&mut app, &owner, requester).await;
    let answer = compose_answer(
        &mut app,
        &mut events,
        "unrelated thread",
        AgentQuestionDelivery::Direct,
    );
    requests.lock().unwrap().clear();
    app.reconnect.offline = true;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    Box::pin(app.handle_event(&mut tui, &mut owner, AppEvent::AnswerAgentQuestion(answer))).await?;
    assert!(recorded_params(&requests, "agent/control").is_empty());
    app.reconnect.offline = false;
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Enter));
    let answer = take_answer(&mut events);
    other_requests.lock().unwrap().clear();
    app.answer_agent_question(&mut other, answer).await;
    assert!(recorded_params(&other_requests, "agent/control").is_empty());
    // A retry uses the existing answer draft and original owner, never the selected backend.
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Enter));
    let retry = take_answer(&mut events);
    assert_eq!(retry.binding.backend_id, owner.backend_id());
    app.answer_agent_question(&mut owner, retry).await;
    assert_eq!(recorded_params(&requests, "agent/control").len(), 1);
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::AnswerAgentQuestion(_))),
        "a definite ownership refusal must preserve the answer and allow an explicit retry"
    );
    owner.shutdown().await?;
    proxy.await??;
    other.shutdown().await?;
    other_proxy.await??;
    Ok(())
}

#[tokio::test]
async fn an_answer_from_another_client_retires_a_locally_queued_reply() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    set_test_initial_prompt(&mut app, String::new());
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source = ThreadId::new();
    let requester = ThreadId::new();
    app.primary_thread_id = Some(source);
    display_test_thread(&mut app, source);
    publish_question(&mut app, &server, requester).await;
    let answer = compose_answer(
        &mut app,
        &mut events,
        "Staging",
        AgentQuestionDelivery::Direct,
    );
    let selected = ThreadId::new();
    app.agent_navigation.clear();
    app.thread_event_channels.clear();
    app.primary_thread_id = Some(selected);
    display_test_thread(&mut app, selected);
    app.handle_backend_event(
        &server,
        AppServerSessionEvent {
            backend_id: server.backend_id(),
            event: AppServerEvent::ServerNotification(Box::new(ServerNotification::ItemCompleted(
                ItemCompletedNotification::new(
                    requester.to_string(),
                    "answer-turn".into(),
                    ThreadItemPayload::UserMessage {
                        id: "desktop-answer".into(),
                        client_id: None,
                        content: vec![AppServerUserInput::Text {
                            text: answer.text.clone(),
                            text_elements: Vec::new(),
                        }],
                    },
                    /*started_at_ms*/ None,
                    0,
                ),
            ))),
        },
    )
    .await;
    requests.lock().unwrap().clear();
    app.answer_agent_question(&mut server, answer).await;
    assert!(recorded_params(&requests, "agent/control").is_empty());
    assert!(!app.thread_event_channels.contains_key(&requester));
    assert_eq!(app.current_displayed_thread_id(), Some(selected));
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn an_indeterminate_reply_stays_bound_and_cannot_be_submitted_twice() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    set_test_initial_prompt(&mut app, String::new());
    let (mut server, requests, proxy) = start_recording_app_server_with_realtime_speech(
        &app.config,
        HistoryCapabilities::CurrentWithLiveNotifications,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        RecordingRequestBehavior {
            realtime: RealtimeRequestBehavior::Forward,
            agent_control: AgentControlRequestBehavior::UnknownPromptOutcome,
            blocked_older_turn_metadata: None,
        },
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await?;
    let source = ThreadId::new();
    let requester = ThreadId::new();
    app.primary_thread_id = Some(source);
    display_test_thread(&mut app, source);
    publish_question(&mut app, &server, requester).await;
    let answer = compose_answer(
        &mut app,
        &mut events,
        "Staging",
        AgentQuestionDelivery::Direct,
    );
    app.answer_agent_question(&mut server, answer.clone()).await;
    assert!(app.chat_widget.has_pending_agent_answer(&answer));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| !matches!(event, AppEvent::AnswerAgentQuestion(_)))
    );
    assert_eq!(recorded_params(&requests, "agent/control").len(), 1);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn bound_question_admission_uses_source_and_requester_authority() -> Result<()> {
    for (quarantined, provider_error) in [
        ("source", true),
        ("requester", true),
        ("source", false),
        ("requester", false),
        ("displayed", true),
    ] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        set_test_initial_prompt(&mut app, String::new());
        let (mut server, requests, proxy) = start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
        let source = ThreadId::new();
        let requester = ThreadId::new();
        let selected = ThreadId::new();
        app.primary_thread_id = Some(source);
        display_test_thread(&mut app, source);
        publish_question(&mut app, &server, requester).await;
        let answer = compose_answer(
            &mut app,
            &mut events,
            "Keep this bound answer",
            AgentQuestionDelivery::Direct,
        );
        display_test_thread(&mut app, selected);
        app.chat_widget
            .restore_user_message_to_composer("Keep the displayed draft".into());
        let error = codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
            model_provider: "unavailable-provider".into(),
        };
        if quarantined == "displayed" {
            app.chat_widget
                .show_provider_error(error, /*rejected_message_id*/ None);
            assert!(!app.chat_widget.is_external_writer_view());
        } else {
            let blocked = if quarantined == "source" {
                source
            } else {
                requester
            };
            if provider_error {
                let mut saved = app
                    .chat_widget
                    .capture_thread_input_state()
                    .expect("input state");
                saved.provider_error = Some(error);
                app.ensure_thread_channel(blocked)
                    .store
                    .lock()
                    .await
                    .input_state = Some(saved);
            } else {
                app.ensure_thread_channel(blocked).mark_external_writer();
            }
        }
        requests.lock().expect("recorder").clear();
        app.answer_agent_question(&mut server, answer.clone()).await;
        assert_eq!(
            recorded_params(&requests, "agent/control").len(),
            usize::from(quarantined == "displayed"),
        );
        assert!(recorded_params(&requests, "turn/start").is_empty());
        assert_eq!(app.current_displayed_thread_id(), Some(selected));
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "Keep the displayed draft"
        );
        if quarantined != "displayed" {
            // A definite local refusal releases the same retained answer for explicit retry.
            app.chat_widget
                .handle_key_event(KeyEvent::from(KeyCode::Enter));
            let retry = take_answer(&mut events);
            assert_eq!(retry, answer);
        }
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}
