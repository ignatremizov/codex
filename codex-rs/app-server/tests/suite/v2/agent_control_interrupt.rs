use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_models_cache;
use codex_app_server_protocol::AgentControlAction;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentControlParams;
use codex_app_server_protocol::AgentControlResponse;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn user_control_interrupt_admits_structured_follow_up_in_v1_and_v2() -> Result<()> {
    for multi_agent_v2 in [false, true] {
        user_control_interrupt_admits_structured_follow_up(multi_agent_v2).await?;
    }
    Ok(())
}

async fn user_control_interrupt_admits_structured_follow_up(multi_agent_v2: bool) -> Result<()> {
    const INITIAL_PROMPT: &str = "keep this child turn active until interrupted";
    const FOLLOW_UP_PROMPT: &str = "continue with this user-authored interrupt follow-up";

    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (initial_gate_tx, initial_gate_rx) = oneshot::channel();
    let mut initial_turn = server
        .mount_response(
            |request| request.body_contains_text(INITIAL_PROMPT),
            vec![StreamingSseChunk {
                gate: Some(initial_gate_rx),
                body: responses::sse(vec![
                    responses::ev_response_created("resp-user-control-interrupt-initial"),
                    responses::ev_assistant_message(
                        "msg-user-control-interrupt-initial",
                        "initial turn should be interrupted",
                    ),
                    responses::ev_completed("resp-user-control-interrupt-initial"),
                ]),
            }],
        )
        .await;
    let mut follow_up_turn = server
        .mount_response(
            |request| request.body_contains_text(FOLLOW_UP_PROMPT),
            vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    responses::ev_response_created("resp-user-control-interrupt-follow-up"),
                    responses::ev_assistant_message(
                        "msg-user-control-interrupt-follow-up",
                        "interrupt follow-up completed",
                    ),
                    responses::ev_completed("resp-user-control-interrupt-follow-up"),
                ]),
            }],
        )
        .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(server.uri());
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
                    model: None,
                    reasoning_effort: None,
                    input: Some(vec![UserInput::Text {
                        text: INITIAL_PROMPT.to_string(),
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
        target_thread_id, ..
    } = spawned.outcome
    else {
        panic!("user control should spawn an active child");
    };
    let initial_request = timeout(DEFAULT_READ_TIMEOUT, initial_turn.wait_for_request()).await?;
    assert!(initial_request.body_contains_text(INITIAL_PROMPT));

    let interrupted: AgentControlResponse = app
        .request(|request_id| ClientRequest::AgentControl {
            request_id,
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("2".to_string()),
                action: AgentControlAction::Interrupt {
                    target: "2".to_string(),
                    input: Some(vec![UserInput::Text {
                        text: FOLLOW_UP_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }]),
                    response_handling: Some(AgentResponseHandling::CommentaryPresentation),
                },
            },
        })
        .await?;
    assert_eq!(interrupted.audit_warning, None);
    let AgentControlOutcome::Interrupted {
        target_thread_id: interrupted_thread_id,
        submission_id,
        post_admission_warning,
    } = interrupted.outcome
    else {
        panic!("user control should interrupt the active child");
    };
    assert_eq!(interrupted_thread_id, target_thread_id);
    assert!(submission_id.is_some());
    assert_eq!(post_admission_warning, None);

    drop(initial_gate_tx);
    let follow_up_request =
        timeout(DEFAULT_READ_TIMEOUT, follow_up_turn.wait_for_request()).await?;
    assert!(follow_up_request.body_contains_text(FOLLOW_UP_PROMPT));
    assert!(
        follow_up_request.body_json()["input"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["type"].as_str() != Some("agent_message"))
    );
    timeout(DEFAULT_READ_TIMEOUT, follow_up_turn.wait_for_completion()).await?;

    let audit = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if matches!(
                completed.item,
                ThreadItem::UserAgentControl {
                    action: UserAgentControlAction::Interrupt,
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
        audit,
        ThreadItem::UserAgentControl {
            action: UserAgentControlAction::Interrupt,
            authored_selector: Some(ref selector),
            target_thread_id: Some(ref audited_target),
            prompt_preview: Some(ref prompt_preview),
            observe_commentary: Some(true),
            final_response: Some(AgentFinalResponseHandling::Presentation),
            status: UserAgentControlStatus::Succeeded,
            ..
        } if selector == "2"
            && audited_target == &target_thread_id
            && prompt_preview == FOLLOW_UP_PROMPT
    ));
    Ok(())
}
